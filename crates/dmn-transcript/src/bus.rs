//! The transcript bus: one publisher per STT worker, many subscribers (detector, session, UI).
//!
//! Bounded and lossy by design. A subscriber that stops draining must never stall an STT worker,
//! so a full subscriber queue drops the segment and counts it; the log every subscriber can hold
//! its own copy of is the durable record, not this channel.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_channel::{Receiver, Sender, TrySendError};

use crate::Segment;

/// Default depth of one subscriber's queue. A minute of dense two-speaker conversation is well
/// under this, so only a stalled subscriber ever loses anything.
pub const DEFAULT_CAPACITY: usize = 256;

#[derive(Default)]
pub struct Bus {
    subscribers: Mutex<Vec<Sender<Segment>>>,
    published: AtomicU64,
    dropped: AtomicU64,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new subscriber queue. Dropping the receiver unsubscribes it at the next publish.
    pub fn subscribe(&self) -> Receiver<Segment> {
        self.subscribe_with_capacity(DEFAULT_CAPACITY)
    }

    pub fn subscribe_with_capacity(&self, capacity: usize) -> Receiver<Segment> {
        let (tx, rx) = crossbeam_channel::bounded(capacity.max(1));
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.push(tx);
        }
        rx
    }

    /// Fan `segment` out to every live subscriber. Never blocks.
    pub fn publish(&self, segment: Segment) {
        self.published.fetch_add(1, Ordering::Relaxed);
        let Ok(mut subs) = self.subscribers.lock() else {
            return;
        };
        let mut dropped = 0u64;
        subs.retain(|tx| match tx.try_send(segment.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                dropped += 1;
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
        if dropped > 0 {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
    }

    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
    }

    /// Deliveries lost because a subscriber's queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn subscribers(&self) -> usize {
        self.subscribers.lock().map(|s| s.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use dmn_audio::Source;

    use super::*;

    fn seg(text: &str) -> Segment {
        Segment {
            source: Source::Interviewer,
            start: Duration::ZERO,
            end: Duration::from_secs(1),
            text: text.to_owned(),
            is_final: true,
            avg_logprob: -0.2,
            language: None,
        }
    }

    #[test]
    fn every_subscriber_gets_every_segment() {
        let bus = Bus::new();
        let a = bus.subscribe();
        let b = bus.subscribe();
        bus.publish(seg("one"));
        bus.publish(seg("two"));
        assert_eq!(
            a.try_iter().map(|s| s.text).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!(
            b.try_iter().map(|s| s.text).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!((bus.published(), bus.dropped()), (2, 0));
    }

    #[test]
    fn a_full_subscriber_loses_segments_without_blocking_the_publisher() {
        let bus = Bus::new();
        let slow = bus.subscribe_with_capacity(2);
        let fast = bus.subscribe_with_capacity(8);
        for i in 0..5 {
            bus.publish(seg(&i.to_string()));
        }
        assert_eq!(slow.try_iter().count(), 2);
        assert_eq!(
            fast.try_iter().count(),
            5,
            "one slow reader hurts nobody else"
        );
        assert_eq!(bus.dropped(), 3);
    }

    #[test]
    fn dropping_a_receiver_unsubscribes_it() {
        let bus = Bus::new();
        let keep = bus.subscribe();
        {
            let _gone = bus.subscribe();
            assert_eq!(bus.subscribers(), 2);
        }
        bus.publish(seg("after"));
        assert_eq!(bus.subscribers(), 1);
        assert_eq!(keep.try_iter().count(), 1);
    }
}
