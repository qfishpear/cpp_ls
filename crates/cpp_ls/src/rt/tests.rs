use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use tokio::sync::{Notify, mpsc};

use super::{Clock, Executor, Next, VirtualClock, Wake};

fn virtual_pair() -> (Executor, VirtualClock) {
    let clock = VirtualClock::new();
    let executor = Executor::new(Clock::Virtual(clock.clone()));
    (executor, clock)
}

#[test]
fn a_real_clock_with_nothing_queued_is_idle() {
    let executor = Executor::new(Clock::Real);
    assert_eq!(executor.drive(Wake::Timers), Next::Idle);
}

#[test]
fn yield_is_polled_once_per_turn_and_only_timers_and_turns_wakes_it() {
    let (executor, _) = virtual_pair();
    let polls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&polls);
    executor.spawn(async move {
        loop {
            count.fetch_add(1, Ordering::Relaxed);
            super::yield_now().await;
        }
    });

    assert_eq!(executor.drive(Wake::TimersAndTurns), Next::Again);
    assert_eq!(polls.load(Ordering::Relaxed), 1);
    assert_eq!(executor.drive(Wake::Timers), Next::Again);
    assert_eq!(polls.load(Ordering::Relaxed), 1);
    assert_eq!(executor.drive(Wake::TimersAndTurns), Next::Again);
    assert_eq!(polls.load(Ordering::Relaxed), 2);
}

#[test]
fn a_task_spawned_during_poll_runs_until_its_first_await() {
    let (executor, _) = virtual_pair();
    let reached = Arc::new(AtomicBool::new(false));
    let after = Arc::new(AtomicBool::new(false));
    let reached_flag = Arc::clone(&reached);
    let after_flag = Arc::clone(&after);
    executor.spawn(async move {
        super::spawn(async move {
            reached_flag.store(true, Ordering::Relaxed);
            super::yield_now().await;
            after_flag.store(true, Ordering::Relaxed);
        });
    });
    assert_eq!(executor.drive(Wake::Timers), Next::Again);
    assert!(reached.load(Ordering::Relaxed));
    assert!(!after.load(Ordering::Relaxed));
}

#[test]
fn a_timer_that_comes_due_during_the_turn_waits_for_the_next_one() {
    let (executor, clock) = virtual_pair();
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    executor.spawn(async move {
        super::sleep(Duration::from_millis(10)).await;
        flag.store(true, Ordering::Relaxed);
    });
    let clock_in_turn = clock.clone();
    executor.spawn(async move {
        clock_in_turn.advance(Duration::from_millis(10));
    });

    assert_eq!(executor.drive(Wake::Timers), Next::After(Duration::ZERO));
    assert!(!done.load(Ordering::Relaxed));
    assert_eq!(executor.drive(Wake::Timers), Next::Idle);
    assert!(done.load(Ordering::Relaxed));
}

#[test]
fn a_sleep_registered_at_the_watermark_does_not_fire_in_the_same_turn() {
    let (executor, _) = virtual_pair();
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    executor.spawn(async move {
        super::sleep(Duration::ZERO).await;
        flag.store(true, Ordering::Relaxed);
    });
    assert_eq!(executor.drive(Wake::Timers), Next::After(Duration::ZERO));
    assert!(!done.load(Ordering::Relaxed));
    assert_eq!(executor.drive(Wake::Timers), Next::Idle);
    assert!(done.load(Ordering::Relaxed));
}

#[test]
fn a_fired_sleep_that_is_still_held_is_not_a_deadline() {
    let (executor, clock) = virtual_pair();
    executor.spawn(async {
        let mut held = Box::pin(super::executor::sleep_on(10_000_000));
        (&mut held).await;
        // `held` stays alive and fired. The host must not be told to come back for it.
        std::future::pending::<()>().await;
        drop(held);
    });
    assert_eq!(
        executor.drive(Wake::Timers),
        Next::After(Duration::from_millis(10))
    );
    clock.advance(Duration::from_millis(10));
    assert_eq!(executor.drive(Wake::Timers), Next::Idle);
}

#[test]
fn dropping_a_sleep_drops_its_deadline() {
    let (executor, _) = virtual_pair();
    executor.spawn(async {
        tokio::select! {
            _ = super::sleep(Duration::from_secs(30)) => {}
            _ = std::future::ready(()) => {}
        }
        super::sleep(Duration::from_millis(5)).await;
    });
    assert_eq!(
        executor.drive(Wake::Timers),
        Next::After(Duration::from_millis(5))
    );
}

#[test]
fn idle_until_a_notify_and_then_the_task_finishes() {
    let (executor, _) = virtual_pair();
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    let notify = Arc::new(Notify::new());
    let waiting = Arc::clone(&notify);
    executor.spawn(async move {
        waiting.notified().await;
        flag.store(true, Ordering::Relaxed);
    });
    assert_eq!(executor.drive(Wake::Timers), Next::Idle);
    assert!(!done.load(Ordering::Relaxed));
    notify.notify_one();
    assert_eq!(executor.drive(Wake::Timers), Next::Idle);
    assert!(done.load(Ordering::Relaxed));
}

#[test]
fn tokio_sync_mpsc_completes_without_a_runtime() {
    let (executor, _) = virtual_pair();
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    executor.spawn(async move {
        assert_eq!(receiver.recv().await, Some(7));
        flag.store(true, Ordering::Relaxed);
    });
    assert_eq!(executor.drive(Wake::Timers), Next::Idle);
    sender.send(7).unwrap();
    assert_eq!(executor.drive(Wake::Timers), Next::Idle);
    assert!(done.load(Ordering::Relaxed));
}

#[test]
fn timeout_follows_the_virtual_clock() {
    let (executor, clock) = virtual_pair();
    let timed_out = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&timed_out);
    let notify = Notify::new();
    executor.spawn(async move {
        let result = super::timeout(Duration::from_secs(1), notify.notified()).await;
        *slot.lock().unwrap() = Some(result.is_err());
    });

    assert_eq!(
        executor.drive(Wake::Timers),
        Next::After(Duration::from_secs(1))
    );
    clock.advance(Duration::from_millis(400));
    assert_eq!(
        executor.drive(Wake::Timers),
        Next::After(Duration::from_millis(600))
    );
    assert!(timed_out.lock().unwrap().is_none());
    clock.advance(Duration::from_millis(600));
    assert_eq!(executor.drive(Wake::Timers), Next::Idle);
    assert_eq!(*timed_out.lock().unwrap(), Some(true));
}

#[test]
fn root_completion_outranks_a_yield() {
    let (executor, _) = virtual_pair();
    executor.spawn_root(async {
        super::spawn(async {
            super::yield_now().await;
        });
    });
    assert_eq!(executor.drive(Wake::Timers), Next::Ended);
}

#[test]
#[should_panic(expected = "re-entered executor drive")]
fn reentering_drive_panics() {
    let (executor, _) = virtual_pair();
    let again = executor.clone();
    executor.spawn(async move {
        let _ = again.drive(Wake::Timers);
    });
    executor.drive(Wake::Timers);
}

#[test]
#[should_panic(expected = "without stalling")]
fn a_self_waking_task_hits_the_poll_limit() {
    let (executor, _) = virtual_pair();
    executor.spawn(async {
        std::future::poll_fn(|cx| {
            cx.waker().wake_by_ref();
            Poll::<()>::Pending
        })
        .await;
    });
    executor.drive(Wake::Timers);
}

#[test]
fn a_panic_in_drive_clears_the_guard() {
    let (executor, _) = virtual_pair();
    executor.spawn(async {
        panic!("task boom");
    });
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        executor.drive(Wake::Timers);
    }));
    assert!(panicked.is_err());
    assert!(!super::on_executor());

    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    executor.spawn(async move {
        super::yield_now().await;
        flag.store(true, Ordering::Relaxed);
    });
    assert_eq!(executor.drive(Wake::TimersAndTurns), Next::Again);
    assert!(!done.load(Ordering::Relaxed));
    assert_eq!(executor.drive(Wake::TimersAndTurns), Next::Idle);
    assert!(done.load(Ordering::Relaxed));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        assert!(!super::on_executor());
        super::yield_now().await;
        super::sleep(Duration::from_millis(1)).await;
    });
}

#[test]
fn offload_runs_inline_and_its_panic_escapes() {
    let (executor, _) = virtual_pair();
    let seen = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&seen);
    executor.spawn(async move {
        let value = super::offload(move || {
            flag.fetch_add(1, Ordering::Relaxed);
            7
        })
        .await;
        assert_eq!(value, 7);
    });
    assert_eq!(executor.drive(Wake::Timers), Next::Idle);
    assert_eq!(seen.load(Ordering::Relaxed), 1);

    let (executor, _) = virtual_pair();
    executor.spawn(async {
        super::offload(|| panic!("offload boom")).await;
    });
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        executor.drive(Wake::Timers);
    }));
    assert!(panicked.is_err());
}

#[tokio::test(flavor = "multi_thread")]
#[should_panic(expected = "offload boom")]
async fn offload_panic_resumes_on_the_runtime() {
    assert!(!super::on_executor());
    super::offload(|| panic!("offload boom")).await;
}
