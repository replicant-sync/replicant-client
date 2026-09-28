//! The core's timers. Scheduling an id that is already pending replaces it.

use std::collections::HashMap;
use std::future::poll_fn;
use std::task::Poll;
use std::time::Duration;

use tokio_util::time::{delay_queue, DelayQueue};

use crate::engine::machine::TimerId;

#[derive(Default)]
pub struct Timers {
    queue: DelayQueue<TimerId>,
    keys: HashMap<TimerId, delay_queue::Key>,
}

impl Timers {
    pub fn schedule(&mut self, timer: TimerId, after: Duration) {
        match self.keys.get(&timer) {
            Some(key) => self.queue.reset(key, after),
            None => {
                let key = self.queue.insert(timer.clone(), after);
                self.keys.insert(timer, key);
            }
        }
    }

    pub fn cancel(&mut self, timer: &TimerId) {
        if let Some(key) = self.keys.remove(timer) {
            self.queue.remove(&key);
        }
    }

    /// Resolves with the next due timer. Pending while none are scheduled; cancel-safe.
    pub async fn next(&mut self) -> TimerId {
        poll_fn(|cx| match self.queue.poll_expired(cx) {
            Poll::Ready(Some(expired)) => {
                let timer = expired.into_inner();
                self.keys.remove(&timer);
                Poll::Ready(timer)
            }
            // An empty queue keeps the waker and wakes it on the next insert.
            Poll::Ready(None) | Poll::Pending => Poll::Pending,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Instant};

    const LONG: Duration = Duration::from_secs(3600);

    #[tokio::test(start_paused = true)]
    async fn fires_in_due_order() {
        let mut timers = Timers::default();
        timers.schedule(TimerId::Pump, Duration::from_millis(200));
        timers.schedule(TimerId::Heartbeat, Duration::from_millis(100));
        assert_eq!(timers.next().await, TimerId::Heartbeat);
        assert_eq!(timers.next().await, TimerId::Pump);
    }

    #[tokio::test(start_paused = true)]
    async fn rescheduling_replaces_the_pending_timer() {
        let mut timers = Timers::default();
        let start = Instant::now();
        timers.schedule(TimerId::Pump, Duration::from_millis(100));
        timers.schedule(TimerId::Pump, Duration::from_millis(500));
        assert_eq!(timers.next().await, TimerId::Pump);
        assert!(start.elapsed() >= Duration::from_millis(500));
        assert!(
            timeout(LONG, timers.next()).await.is_err(),
            "a replaced timer fires once"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_timer_never_fires() {
        let mut timers = Timers::default();
        timers.schedule(TimerId::Reconnect, Duration::from_millis(100));
        timers.cancel(&TimerId::Reconnect);
        timers.cancel(&TimerId::HaltRetry);
        assert!(timeout(LONG, timers.next()).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn ids_with_different_payloads_are_independent() {
        let mut timers = Timers::default();
        timers.schedule(TimerId::Request(1), Duration::from_millis(100));
        timers.schedule(TimerId::Request(2), Duration::from_millis(200));
        timers.cancel(&TimerId::Request(1));
        assert_eq!(timers.next().await, TimerId::Request(2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_fired_timer_can_be_scheduled_again() {
        let mut timers = Timers::default();
        timers.schedule(TimerId::Heartbeat, Duration::from_secs(30));
        assert_eq!(timers.next().await, TimerId::Heartbeat);
        timers.schedule(TimerId::Heartbeat, Duration::from_secs(30));
        assert_eq!(timers.next().await, TimerId::Heartbeat);
    }
}
