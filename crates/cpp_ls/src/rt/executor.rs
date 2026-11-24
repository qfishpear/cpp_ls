//! A turn-based executor. `drive` polls ready tasks until every one of them is waiting, then
//! returns. It does not park.

#![cfg_attr(not(test), allow(dead_code))]

use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_task::Runnable;

/// Which waits a turn is allowed to release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
    /// Timers whose deadline is at or before the clock captured on entry.
    Timers,
    /// Those timers, plus every `yield_now` registered by the previous turn.
    TimersAndTurns,
}

/// What the host should do after a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Next {
    /// The root task finished.
    Ended,
    /// A task yielded. Come back on the next macrotask. Timers do not change this.
    Again,
    /// No yield is waiting. `Duration::ZERO` means a timer is already due.
    After(Duration),
    /// Nothing is waiting on a timer or a yield. Only a new wake moves the executor.
    Idle,
}

/// Clock handed to [`Executor::new`].
#[derive(Clone, Debug)]
pub(crate) enum Clock {
    Virtual(VirtualClock),
    /// `std::time::Instant`. On wasip1 that read is `clock_time_get`.
    Real,
}

/// Nanoseconds since an arbitrary zero. Advanced by the handle, including from outside `drive`.
#[derive(Clone, Debug)]
pub(crate) struct VirtualClock {
    nanos: Arc<AtomicU64>,
}

impl VirtualClock {
    pub(crate) fn new() -> Self {
        Self {
            nanos: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn advance(&self, by: Duration) {
        let add = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        let _ = self.nanos.fetch_update(
            AtomicOrdering::Relaxed,
            AtomicOrdering::Relaxed,
            |current| Some(current.saturating_add(add)),
        );
    }

    fn now_nanos(&self) -> u64 {
        self.nanos.load(AtomicOrdering::Relaxed)
    }
}

enum ClockKind {
    Virtual(VirtualClock),
    Real { origin: std::time::Instant },
}

struct TimerState {
    fired: AtomicBool,
    cancelled: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

struct TimerEntry {
    deadline: u64,
    id: u64,
    state: Arc<TimerState>,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.id == other.id
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // `BinaryHeap` pops the greatest entry. The soonest deadline has to win.
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.id.cmp(&self.id))
    }
}

struct Inner {
    clock: ClockKind,
    ready: Mutex<VecDeque<Runnable>>,
    yields: Mutex<Vec<Waker>>,
    timers: Mutex<BinaryHeap<TimerEntry>>,
    next_timer: AtomicU64,
    root_done: Arc<AtomicBool>,
    root_set: AtomicBool,
}

impl Inner {
    fn now_nanos(&self) -> u64 {
        match &self.clock {
            ClockKind::Virtual(clock) => clock.now_nanos(),
            ClockKind::Real { origin } => {
                u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
            }
        }
    }

    fn enqueue(self: &Arc<Self>, future: impl Future<Output = ()> + Send + 'static) {
        let queue = Arc::downgrade(self);
        let (runnable, task) = async_task::spawn(future, move |runnable| {
            let Some(inner) = queue.upgrade() else {
                drop(runnable);
                return;
            };
            lock(&inner.ready).push_back(runnable);
        });
        task.detach();
        runnable.schedule();
    }

    fn arm(&self, wake: Wake) {
        let watermark = self.now_nanos();
        let mut due = Vec::new();
        {
            // The heap keeps the `Arc` until the `Sleep` drops it. Popping here would free the
            // state before the task observes `fired`, and the same sleep would register again.
            let mut heap = lock(&self.timers);
            let pending = std::mem::take(&mut *heap);
            for entry in pending {
                if entry.state.cancelled.load(AtomicOrdering::Acquire) {
                    continue;
                }
                if !entry.state.fired.load(AtomicOrdering::Acquire) && entry.deadline <= watermark {
                    entry.state.fired.store(true, AtomicOrdering::Release);
                    due.push(Arc::clone(&entry.state));
                }
                heap.push(entry);
            }
        }
        for state in due {
            if let Some(waker) = lock(&state.waker).take() {
                waker.wake();
            }
        }
        if wake == Wake::TimersAndTurns {
            let yielded = std::mem::take(&mut *lock(&self.yields));
            for waker in yielded {
                waker.wake();
            }
        }
    }

    fn run_until_stalled(&self) {
        // A task that wakes itself on every poll never stalls. Release builds have no cap, so no counter.
        #[cfg(any(test, debug_assertions))]
        let mut polls = 0u32;
        loop {
            let Some(runnable) = lock(&self.ready).pop_front() else {
                break;
            };
            #[cfg(any(test, debug_assertions))]
            {
                polls += 1;
                if polls > 100_000 {
                    panic!("executor polled {polls} times without stalling");
                }
            }
            runnable.run();
        }
    }

    fn outcome(&self) -> Next {
        if self.root_done.load(AtomicOrdering::Acquire) {
            return Next::Ended;
        }
        if !lock(&self.yields).is_empty() {
            return Next::Again;
        }
        let now = self.now_nanos();
        match next_deadline(&self.timers) {
            Some(deadline) => Next::After(Duration::from_nanos(deadline.saturating_sub(now))),
            None => Next::Idle,
        }
    }

    fn register_yield(&self, waker: Waker) {
        lock(&self.yields).push(waker);
    }
}

fn next_deadline(timers: &Mutex<BinaryHeap<TimerEntry>>) -> Option<u64> {
    let mut heap = lock(timers);
    // A fired timer stays in the heap until its `Sleep` drops, and it never fires twice. Reporting its past deadline
    // would tell the host to come straight back, every macrotask, for as long as the sleep is held.
    heap.retain(|entry| !entry.state.cancelled.load(AtomicOrdering::Acquire));
    heap.iter()
        .filter(|entry| !entry.state.fired.load(AtomicOrdering::Acquire))
        .map(|entry| entry.deadline)
        .min()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One reactor session's tasks. Clone shares the queues; `drive` is still one at a time per thread.
#[derive(Clone)]
pub(crate) struct Executor {
    inner: Arc<Inner>,
}

impl Executor {
    pub(crate) fn new(clock: Clock) -> Self {
        let clock = match clock {
            Clock::Virtual(clock) => ClockKind::Virtual(clock),
            Clock::Real => ClockKind::Real {
                origin: std::time::Instant::now(),
            },
        };
        Self {
            inner: Arc::new(Inner {
                clock,
                ready: Mutex::new(VecDeque::new()),
                yields: Mutex::new(Vec::new()),
                timers: Mutex::new(BinaryHeap::new()),
                next_timer: AtomicU64::new(1),
                root_done: Arc::new(AtomicBool::new(false)),
                root_set: AtomicBool::new(false),
            }),
        }
    }

    /// Queue a task on this executor. Safe to call before `drive`; `rt::spawn` from inside a
    /// running task reaches the same queue through the thread-local guard.
    pub(crate) fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.inner.enqueue(future);
    }

    /// The session task. Its completion makes every later turn report [`Next::Ended`].
    pub(crate) fn spawn_root(&self, future: impl Future<Output = ()> + Send + 'static) {
        if self.inner.root_set.swap(true, AtomicOrdering::Relaxed) {
            panic!("executor root already set");
        }
        let done = Arc::clone(&self.inner.root_done);
        self.spawn(async move {
            future.await;
            done.store(true, AtomicOrdering::Release);
        });
    }

    pub(crate) fn drive(&self, wake: Wake) -> Next {
        let _guard = enter(Arc::clone(&self.inner));
        self.inner.arm(wake);
        self.inner.run_until_stalled();
        self.inner.outcome()
    }
}

pub(super) fn executor_now_nanos() -> u64 {
    current().expect("clock outside the executor").now_nanos()
}

pub(super) fn enqueue_on_current(future: impl Future<Output = ()> + Send + 'static) {
    current()
        .expect("spawn outside the executor")
        .enqueue(future);
}

pub(super) fn sleep_on(deadline_nanos: u64) -> impl Future<Output = ()> {
    ExecutorSleep {
        deadline: deadline_nanos,
        state: Weak::new(),
    }
}

pub(super) fn register_yield(waker: Waker) {
    current()
        .expect("yield outside the executor")
        .register_yield(waker);
}

struct ExecutorSleep {
    deadline: u64,
    state: Weak<TimerState>,
}

impl Future for ExecutorSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if let Some(state) = this.state.upgrade() {
            if state.fired.load(AtomicOrdering::Acquire) {
                return Poll::Ready(());
            }
            *lock(&state.waker) = Some(cx.waker().clone());
            if state.fired.load(AtomicOrdering::Acquire) {
                return Poll::Ready(());
            }
            return Poll::Pending;
        }

        let inner = current().expect("sleep outside the executor");
        // A timer born during this turn is not swept, even when its deadline is already at or
        // behind the watermark. The next turn's opening sweep fires it.
        let state = Arc::new(TimerState {
            fired: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            waker: Mutex::new(Some(cx.waker().clone())),
        });
        this.state = Arc::downgrade(&state);
        let id = inner.next_timer.fetch_add(1, AtomicOrdering::Relaxed);
        lock(&inner.timers).push(TimerEntry {
            deadline: this.deadline,
            id,
            state,
        });
        Poll::Pending
    }
}

impl Drop for ExecutorSleep {
    fn drop(&mut self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        state.cancelled.store(true, AtomicOrdering::Release);
        lock(&state.waker).take();
    }
}

thread_local! {
    static CURRENT: std::cell::RefCell<Option<Arc<Inner>>> = const { std::cell::RefCell::new(None) };
}

struct DriveGuard;

impl Drop for DriveGuard {
    fn drop(&mut self) {
        CURRENT.with(|slot| *slot.borrow_mut() = None);
    }
}

fn enter(inner: Arc<Inner>) -> DriveGuard {
    CURRENT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            panic!("re-entered executor drive");
        }
        *slot = Some(inner);
    });
    DriveGuard
}

fn current() -> Option<Arc<Inner>> {
    CURRENT.with(|slot| slot.borrow().clone())
}

pub(super) fn on_executor() -> bool {
    current().is_some()
}
