//! Async primitives that follow whoever is driving the thread.
//!
//! Inside [`executor::Executor::drive`] the calls land on the turn executor. Outside it they are
//! tokio, so `tokio::time::pause()` still stops sleeps. The wasm reactor has no tokio runtime, so
//! those same calls panic: a sleep there would ask the host for `poll_oneoff`, which this reactor
//! does not import. The executor guard lives only for the `drive` stack frame and clears itself on
//! unwind, which lets one thread alternate sessions and ordinary `#[tokio::test]`s.

#![cfg_attr(not(test), allow(dead_code, unused_imports))]

mod executor;

use std::future::Future;
use std::ops::Add;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

pub(crate) use executor::{Clock, Executor, Next, VirtualClock, Wake};

/// Monotonic instant. On the executor this is that executor's clock. On tokio it is an offset from
/// a thread-local `tokio::time::Instant`, so a paused runtime does not advance it.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub(crate) struct Instant {
    nanos: u64,
}

impl Instant {
    pub(crate) fn saturating_duration_since(&self, earlier: Instant) -> Duration {
        Duration::from_nanos(self.nanos.saturating_sub(earlier.nanos))
    }

    pub(crate) fn checked_add(self, duration: Duration) -> Option<Instant> {
        let add = u64::try_from(duration.as_nanos()).ok()?;
        self.nanos.checked_add(add).map(|nanos| Instant { nanos })
    }
}

impl Add<Duration> for Instant {
    type Output = Self;

    fn add(self, duration: Duration) -> Self {
        let add = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        Self {
            nanos: self.nanos.saturating_add(add),
        }
    }
}

pub(crate) fn on_executor() -> bool {
    executor::on_executor()
}

pub(crate) fn now() -> Instant {
    if on_executor() {
        Instant {
            nanos: executor::executor_now_nanos(),
        }
    } else {
        now_outside()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn now_outside() -> Instant {
    Instant {
        nanos: tokio_now_nanos(),
    }
}

#[cfg(target_arch = "wasm32")]
fn now_outside() -> Instant {
    panic!("rt called outside drive");
}

pub(crate) async fn sleep(duration: Duration) {
    sleep_until(now() + duration).await;
}

pub(crate) async fn sleep_until(deadline: Instant) {
    if on_executor() {
        executor::sleep_on(deadline.nanos).await;
    } else {
        sleep_until_outside(deadline).await;
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn sleep_until_outside(deadline: Instant) {
    let wait = deadline.saturating_duration_since(now());
    tokio::time::sleep(wait).await;
}

#[cfg(target_arch = "wasm32")]
async fn sleep_until_outside(_deadline: Instant) {
    panic!("rt called outside drive");
}

/// `Err` when `duration` elapses first.
pub(crate) async fn timeout<F>(duration: Duration, future: F) -> Result<F::Output, Elapsed>
where
    F: Future,
{
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => Ok(result),
        _ = sleep(duration) => Err(Elapsed),
    }
}

#[derive(Debug)]
pub(crate) struct Elapsed;

/// Fire-and-forget. `Send` is required so the tokio path can take the future. The executor itself
/// polls on the driving thread only.
pub(crate) fn spawn(future: impl Future<Output = ()> + Send + 'static) {
    if on_executor() {
        executor::enqueue_on_current(future);
    } else {
        spawn_outside(future);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_outside(future: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(future);
}

#[cfg(target_arch = "wasm32")]
fn spawn_outside(_future: impl Future<Output = ()> + Send + 'static) {
    panic!("rt called outside drive");
}

pub(crate) async fn yield_now() {
    if on_executor() {
        Yield { polled: false }.await;
    } else {
        yield_outside().await;
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn yield_outside() {
    tokio::task::yield_now().await;
}

#[cfg(target_arch = "wasm32")]
async fn yield_outside() {
    panic!("rt called outside drive");
}

struct Yield {
    polled: bool,
}

impl Future for Yield {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.polled {
            return Poll::Ready(());
        }
        self.polled = true;
        // Stay pending without waking. The next `TimersAndTurns` drive releases the waker.
        executor::register_yield(cx.waker().clone());
        Poll::Pending
    }
}

/// Run `f` where blocking is allowed.
///
/// On the executor `f` runs inline and its panic propagates. On a native tokio runtime it is
/// `spawn_blocking`; a panic comes back out through `resume_unwind`. Wasm has no tokio runtime,
/// so calling this outside `drive` panics.
pub(crate) async fn offload<R, F>(f: F) -> R
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    if on_executor() {
        return f();
    }
    offload_outside(f).await
}

#[cfg(not(target_arch = "wasm32"))]
async fn offload_outside<R, F>(f: F) -> R
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => value,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => panic!("offload failed: {error}"),
    }
}

#[cfg(target_arch = "wasm32")]
async fn offload_outside<R, F>(_f: F) -> R
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    panic!("rt called outside drive");
}

#[cfg(not(target_arch = "wasm32"))]
fn tokio_now_nanos() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static ORIGIN: Cell<Option<tokio::time::Instant>> = const { Cell::new(None) };
    }
    let now = tokio::time::Instant::now();
    ORIGIN.with(|cell| {
        if let Some(origin) = cell.get()
            && let Some(elapsed) = now.checked_duration_since(origin)
        {
            return u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        }
        cell.set(Some(now));
        0
    })
}

#[cfg(test)]
mod tests;
