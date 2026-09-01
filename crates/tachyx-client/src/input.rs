//! Turning a stream of input events into the batches that go on the wire.
//!
//! A finger dragging across the glass produces one event per touch sample, which
//! on a 120 Hz phone is 120 events a second that all mean the same thing: the
//! pointer moved. Sending each one costs a packet and a wake-up on both ends, and
//! the host cannot act on them separately anyway, because it adds the deltas
//! together before injecting anything.
//!
//! So consecutive events of the same continuous kind are merged as they arrive,
//! and the result is handed over once a tick. Nothing else is merged: a button
//! press between two motions is a boundary, and merging across it would move the
//! pointer to the wrong place before the click.

use tachyx_proto::input::InputEvent;

use crate::control::{ClientTime, TimedInput};

/// Collects input events and hands them over a batch at a time.
#[derive(Debug, Default)]
pub struct InputQueue {
    pending: Vec<TimedInput>,
}

impl InputQueue {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records what the user did, merging it into the previous event when the
    /// two describe one continuous motion.
    ///
    /// The merged event carries the later timestamp, because that is when the
    /// displacement it describes was complete.
    pub fn push(&mut self, at: ClientTime, event: InputEvent) {
        if let Some(last) = self.pending.last_mut()
            && merge(&mut last.event, &event)
        {
            last.at = at;
            return;
        }
        self.pending.push(TimedInput { at, event });
    }

    /// Takes everything recorded since the last call, or `None` if nothing was.
    pub fn take(&mut self) -> Option<Vec<TimedInput>> {
        if self.pending.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.pending))
        }
    }

    /// Whether anything is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Folds `next` into `into` when the two are one continuous motion, reporting
/// whether it did.
fn merge(into: &mut InputEvent, next: &InputEvent) -> bool {
    match (into, next) {
        (InputEvent::PointerMotion { dx, dy }, InputEvent::PointerMotion { dx: nx, dy: ny })
        | (InputEvent::Wheel { dx, dy }, InputEvent::Wheel { dx: nx, dy: ny })
        | (InputEvent::FingerScroll { dx, dy }, InputEvent::FingerScroll { dx: nx, dy: ny }) => {
            *dx += nx;
            *dy += ny;
            true
        }
        // An absolute position replaces the one before it rather than adding to
        // it. Only the newest says where the pointer is.
        (
            InputEvent::PointerMotionAbsolute { x, y },
            InputEvent::PointerMotionAbsolute { x: nx, y: ny },
        ) => {
            *x = *nx;
            *y = *ny;
            true
        }
        // Text is appended, so that a fast typist's keystrokes cost one event
        // rather than one each. The host inserts the string as a whole.
        (InputEvent::Text(held), InputEvent::Text(more)) => {
            held.push_str(more);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use tachyx_proto::input::{HidUsage, PointerButton, Press};

    use super::*;

    fn at(micros: u64) -> ClientTime {
        ClientTime(micros)
    }

    #[test]
    fn a_burst_of_motion_becomes_one_event() {
        let mut queue = InputQueue::new();
        for step in 1..=10u64 {
            queue.push(
                at(step * 8_000),
                InputEvent::PointerMotion { dx: 3.0, dy: -1.0 },
            );
        }
        let batch = queue.take().expect("ten pushes produce a batch");
        assert_eq!(
            batch,
            vec![TimedInput {
                at: at(80_000),
                event: InputEvent::PointerMotion {
                    dx: 30.0,
                    dy: -10.0
                },
            }],
            "the batch carries the total displacement and the latest instant"
        );
    }

    #[test]
    fn a_click_between_two_motions_is_a_boundary() {
        let mut queue = InputQueue::new();
        queue.push(at(1), InputEvent::PointerMotion { dx: 1.0, dy: 0.0 });
        queue.push(
            at(2),
            InputEvent::PointerButton {
                button: PointerButton::Left,
                press: Press::Down,
            },
        );
        queue.push(at(3), InputEvent::PointerMotion { dx: 5.0, dy: 0.0 });
        let batch = queue.take().expect("three pushes produce a batch");
        assert_eq!(batch.len(), 3, "merging across a click would misplace it");
        assert_eq!(
            batch[2].event,
            InputEvent::PointerMotion { dx: 5.0, dy: 0.0 }
        );
    }

    #[test]
    fn scrolling_and_its_end_stay_separate() {
        let mut queue = InputQueue::new();
        queue.push(at(1), InputEvent::FingerScroll { dx: 0.0, dy: 12.0 });
        queue.push(at(2), InputEvent::FingerScroll { dx: 0.0, dy: 8.0 });
        queue.push(at(3), InputEvent::FingerScrollEnd);
        let batch = queue.take().expect("three pushes produce a batch");
        assert_eq!(
            batch.iter().map(|t| t.event.clone()).collect::<Vec<_>>(),
            vec![
                InputEvent::FingerScroll { dx: 0.0, dy: 20.0 },
                InputEvent::FingerScrollEnd
            ]
        );
    }

    #[test]
    fn motion_and_scroll_are_not_the_same_motion() {
        let mut queue = InputQueue::new();
        queue.push(at(1), InputEvent::PointerMotion { dx: 4.0, dy: 0.0 });
        queue.push(at(2), InputEvent::FingerScroll { dx: 4.0, dy: 0.0 });
        assert_eq!(queue.take().expect("two pushes").len(), 2);
    }

    #[test]
    fn an_absolute_position_replaces_rather_than_accumulates() {
        let mut queue = InputQueue::new();
        queue.push(at(1), InputEvent::PointerMotionAbsolute { x: 0.1, y: 0.1 });
        queue.push(at(2), InputEvent::PointerMotionAbsolute { x: 0.9, y: 0.4 });
        let batch = queue.take().expect("two pushes produce a batch");
        assert_eq!(
            batch,
            vec![TimedInput {
                at: at(2),
                event: InputEvent::PointerMotionAbsolute { x: 0.9, y: 0.4 },
            }]
        );
    }

    #[test]
    fn keys_are_never_merged() {
        let mut queue = InputQueue::new();
        let usage = HidUsage {
            page: HidUsage::KEYBOARD_PAGE,
            id: 0x04,
        };
        queue.push(
            at(1),
            InputEvent::Key {
                usage,
                press: Press::Down,
            },
        );
        queue.push(
            at(2),
            InputEvent::Key {
                usage,
                press: Press::Up,
            },
        );
        assert_eq!(
            queue.take().expect("two pushes").len(),
            2,
            "a key that goes down and up must not collapse into one event"
        );
    }

    #[test]
    fn an_empty_tick_produces_nothing_to_send() {
        let mut queue = InputQueue::new();
        assert!(queue.is_empty());
        assert!(queue.take().is_none());
    }
}
