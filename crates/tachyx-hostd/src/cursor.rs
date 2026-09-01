//! Carrying the pointer from capture to the client.
//!
//! Capture leaves the cursor out of the picture on purpose, so the client draws
//! it. This is what tells the client where to draw it, and it is a correction
//! rather than a source: the client moves its pointer the moment the finger
//! moves and reconciles with what arrives here, so no round trip sits inside a
//! gesture.
//!
//! The picture travels once and the position travels often. They are separated
//! here because both share the reliable stream input travels on, and a picture
//! resent with every step of the pointer would queue in front of the user's
//! next click.

use std::time::Duration;

use tachyx_media::Cursor;
use tachyx_proto::control::{CursorImage, CursorPoint, CursorReport, CursorUpdate, Size};

/// How often the host looks at the pointer.
///
/// One video frame at sixty hertz, so a correction never sits behind the
/// picture it belongs to. Each one is about a dozen bytes, and one that finds
/// nothing changed costs a mutex and no message at all.
pub const INTERVAL: Duration = Duration::from_millis(16);

/// Turns what capture knows about the pointer into what goes on the wire.
#[derive(Debug)]
pub struct CursorFeed {
    screen: Size,
    /// The cursor serial last reported, if any.
    reported: Option<u64>,
    /// The image serial whose picture the client has, if any.
    pictured: Option<u64>,
    /// Whether the client has been told this host cannot track the pointer.
    said_untracked: bool,
    /// Whether a picture too large for a control message has been complained
    /// about, which happens once and not once a frame.
    said_refused: bool,
}

impl CursorFeed {
    /// A feed for a capture of this size.
    ///
    /// `None` for a capture wider or taller than the wire can express, which is
    /// a screen of more than 65535 pixels on a side.
    #[must_use]
    pub fn new(capture: tachyx_media::Size) -> Option<Self> {
        let screen = Size::new(
            u16::try_from(capture.width()).ok()?,
            u16::try_from(capture.height()).ok()?,
        )?;
        Some(Self {
            screen,
            reported: None,
            pictured: None,
            said_untracked: false,
            said_refused: false,
        })
    }

    /// The update to send, or `None` when the client already knows this.
    pub fn poll(&mut self, cursor: &Cursor) -> Option<CursorUpdate> {
        let Some(pointer) = cursor.pointer.as_ref() else {
            // Said once. Nothing follows it, because nothing more is known.
            if std::mem::replace(&mut self.said_untracked, true) {
                return None;
            }
            return Some(CursorUpdate::Untracked);
        };
        if self.reported == Some(cursor.serial) {
            return None;
        }
        self.reported = Some(cursor.serial);

        let image = (self.pictured != Some(cursor.image_serial))
            .then_some(pointer.image.as_ref())
            .flatten()
            .and_then(|picture| self.encode(picture));
        if image.is_some() {
            self.pictured = Some(cursor.image_serial);
        }
        Some(CursorUpdate::Tracked(CursorReport {
            screen: self.screen,
            at: pointer.at.map(|(x, y)| CursorPoint {
                x,
                y,
                hotspot_x: pointer.hotspot.0,
                hotspot_y: pointer.hotspot.1,
            }),
            image,
        }))
    }

    /// The captured picture as the wire carries it, or `None` for one that will
    /// not fit.
    ///
    /// The compositor hands over `ARGB8888`, which is one little-endian word a
    /// pixel and therefore blue, green, red then alpha in memory, with the
    /// colour already multiplied by the alpha. That is what the wire format
    /// says, so the rows are copied rather than converted.
    fn encode(&mut self, picture: &tachyx_media::CursorImage) -> Option<CursorImage> {
        let encoded = u16::try_from(picture.width)
            .ok()
            .and_then(std::num::NonZeroU16::new)
            .zip(
                u16::try_from(picture.height)
                    .ok()
                    .and_then(std::num::NonZeroU16::new),
            )
            .and_then(|(width, height)| CursorImage::new(width, height, picture.pixels.clone()));
        if encoded.is_none() && !std::mem::replace(&mut self.said_refused, true) {
            tracing::warn!(
                width = picture.width,
                height = picture.height,
                "the cursor picture is larger than one control message; the client draws its own"
            );
        }
        encoded
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tachyx_media::Pointer;

    use super::*;

    fn capture() -> tachyx_media::Size {
        tachyx_media::Size::new(3440, 1440).expect("a size")
    }

    fn feed() -> CursorFeed {
        CursorFeed::new(capture()).expect("a screen the wire can express")
    }

    fn arrow(edge: u32, fill: u8) -> Arc<tachyx_media::CursorImage> {
        Arc::new(tachyx_media::CursorImage {
            width: edge,
            height: edge,
            pixels: vec![fill; (edge * edge * 4) as usize],
        })
    }

    fn shown(at: Option<(i32, i32)>, image: Option<Arc<tachyx_media::CursorImage>>) -> Cursor {
        Cursor {
            pointer: Some(Pointer {
                at,
                hotspot: (5, 1),
                image,
            }),
            serial: 1,
            image_serial: 1,
        }
    }

    fn tracked(update: Option<CursorUpdate>) -> CursorReport {
        match update {
            Some(CursorUpdate::Tracked(report)) => report,
            other => panic!("expected a tracked report, got {other:?}"),
        }
    }

    #[test]
    fn a_compositor_that_reports_no_pointer_is_announced_once() {
        let mut feed = feed();
        let untracked = Cursor::default();
        assert_eq!(feed.poll(&untracked), Some(CursorUpdate::Untracked));
        assert_eq!(
            feed.poll(&untracked),
            None,
            "repeating it every tick would spend the stream on a fact that cannot change"
        );
    }

    #[test]
    fn the_first_report_carries_the_picture() {
        let mut feed = feed();
        let report = tracked(feed.poll(&shown(Some((1436, 710)), Some(arrow(48, 0x7f)))));
        assert_eq!(report.screen, Size::new(3440, 1440).expect("a size"));
        assert_eq!(
            report.at,
            Some(CursorPoint {
                x: 1436,
                y: 710,
                hotspot_x: 5,
                hotspot_y: 1,
            })
        );
        assert!(report.image.is_some(), "the client has no picture yet");
    }

    #[test]
    fn a_move_carries_no_picture() {
        let mut feed = feed();
        let picture = arrow(48, 0x7f);
        feed.poll(&shown(Some((10, 10)), Some(Arc::clone(&picture))));

        let mut moved = shown(Some((11, 12)), Some(picture));
        moved.serial = 2;
        let report = tracked(feed.poll(&moved));
        assert_eq!(report.at.map(|at| (at.x, at.y)), Some((11, 12)));
        assert_eq!(
            report.image, None,
            "the picture did not change, so resending it would delay the next click"
        );
    }

    #[test]
    fn nothing_is_sent_while_nothing_changes() {
        let mut feed = feed();
        let still = shown(Some((10, 10)), Some(arrow(48, 0x7f)));
        assert!(feed.poll(&still).is_some());
        assert_eq!(feed.poll(&still), None);
        assert_eq!(feed.poll(&still), None);
    }

    #[test]
    fn a_new_picture_travels_when_the_pointer_crosses_a_widget() {
        let mut feed = feed();
        feed.poll(&shown(Some((10, 10)), Some(arrow(48, 0x7f))));

        let mut beam = shown(Some((10, 10)), Some(arrow(48, 0x22)));
        beam.serial = 2;
        beam.image_serial = 2;
        let report = tracked(feed.poll(&beam));
        let image = report.image.expect("a changed picture travels");
        assert_eq!(image.pixels(), Some(&vec![0x22u8; 48 * 48 * 4][..]));
    }

    #[test]
    fn a_pointer_off_the_captured_screen_is_reported_as_gone() {
        let mut feed = feed();
        feed.poll(&shown(Some((10, 10)), Some(arrow(48, 0x7f))));

        let mut left = shown(None, Some(arrow(48, 0x7f)));
        left.serial = 2;
        assert_eq!(
            tracked(feed.poll(&left)).at,
            None,
            "a pointer on another monitor must not be drawn on this one"
        );
    }

    #[test]
    fn a_picture_too_large_for_a_message_is_dropped_and_the_position_is_not() {
        let mut feed = feed();
        let report = tracked(feed.poll(&shown(Some((10, 10)), Some(arrow(256, 0x7f)))));
        assert_eq!(report.at.map(|at| (at.x, at.y)), Some((10, 10)));
        assert_eq!(report.image, None);

        // The picture was never sent, so a later position must still be able to
        // carry one that does fit.
        let mut smaller = shown(Some((11, 11)), Some(arrow(48, 0x7f)));
        smaller.serial = 2;
        smaller.image_serial = 2;
        assert!(tracked(feed.poll(&smaller)).image.is_some());
    }
}
