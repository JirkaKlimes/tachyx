//! The depth-one hand-off between capture and encode.

use std::sync::{Condvar, Mutex};

/// Holds at most one frame, and never blocks the thread that fills it.
///
/// A queue would be wrong here. Capture runs at the refresh rate and encode
/// takes about a refresh period, so a queue would grow whenever the two
/// disagreed and every frame after that would be delivered late. Replacing
/// instead means the encoder always starts on the newest picture, and the cost
/// of falling behind is a counted drop rather than accumulated latency.
#[derive(Debug)]
pub(crate) struct Slot<T> {
    pending: Mutex<Pending<T>>,
    arrived: Condvar,
}

#[derive(Debug)]
struct Pending<T> {
    frame: Option<T>,
    open: bool,
}

impl<T> Slot<T> {
    pub(crate) fn new() -> Self {
        Self {
            pending: Mutex::new(Pending {
                frame: None,
                open: true,
            }),
            arrived: Condvar::new(),
        }
    }

    /// Publishes a frame, returning the one that will not be consumed: the one
    /// it displaced, or this one if the slot has closed.
    ///
    /// The caller drops that frame, which is what returns its buffer to the
    /// ring, and counts it.
    pub(crate) fn store(&self, frame: T) -> Option<T> {
        let mut pending = self
            .pending
            .lock()
            .expect("the slot mutex is never held across a panic");
        if !pending.open {
            return Some(frame);
        }
        let displaced = pending.frame.replace(frame);
        drop(pending);
        self.arrived.notify_one();
        displaced
    }

    /// Waits for a frame, or for the slot to close.
    pub(crate) fn take(&self) -> Option<T> {
        let mut pending = self
            .pending
            .lock()
            .expect("the slot mutex is never held across a panic");
        loop {
            if let Some(frame) = pending.frame.take() {
                return Some(frame);
            }
            if !pending.open {
                return None;
            }
            pending = self
                .arrived
                .wait(pending)
                .expect("the slot mutex is never held across a panic");
        }
    }

    /// Ends delivery, so a waiting consumer stops rather than blocking forever.
    pub(crate) fn close(&self) {
        self.pending
            .lock()
            .expect("the slot mutex is never held across a panic")
            .open = false;
        self.arrived.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::Slot;

    #[test]
    fn storing_twice_keeps_the_newer_frame() {
        let slot = Slot::new();
        assert!(slot.store(1).is_none());
        assert_eq!(slot.store(2), Some(1));
        assert_eq!(slot.take(), Some(2));
    }

    #[test]
    fn closing_releases_a_waiting_consumer() {
        let slot = Arc::new(Slot::<u8>::new());
        let waiter = {
            let slot = Arc::clone(&slot);
            std::thread::spawn(move || slot.take())
        };
        slot.close();
        assert_eq!(waiter.join().unwrap(), None);
    }

    #[test]
    fn a_frame_stored_before_the_close_is_still_delivered() {
        let slot = Slot::new();
        slot.store(7);
        slot.close();
        assert_eq!(slot.take(), Some(7));
        assert_eq!(slot.take(), None);
    }

    #[test]
    fn a_closed_slot_hands_a_frame_back_rather_than_holding_it() {
        let slot = Slot::new();
        slot.close();
        assert_eq!(slot.store(9), Some(9));
    }
}
