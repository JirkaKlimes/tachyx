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
//!
//! Once, and not once per change: the pointer flips between a handful of shapes
//! as it crosses a window, and a shape it comes back to is one the client still
//! holds, so it is named rather than sent again.
//!
//! Everything here is in the logical pixels the compositor lays the output out
//! in, which on a scaled output are not the pixels the frames are captured in.
//! Nothing is converted: capture already reports the pointer in them, and this
//! only has to name the right screen to measure them against. See
//! [`tachyx_proto::control::CursorReport`].

use std::collections::VecDeque;
use std::time::Duration;

use tachyx_media::Cursor;
use tachyx_proto::control::{
    CursorImage, CursorImageId, CursorPicture, CursorPoint, CursorReport, CursorUpdate, Size,
};

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
    /// The pictures the client has been sent, oldest first.
    ///
    /// Bounded at [`CursorImage::REMEMBERED`] and dropped oldest first, which
    /// is what the client is asked to do with the same list. The two are built
    /// from one ordered stream, so they hold the same identifiers, and naming a
    /// picture is only safe while that is true.
    sent: VecDeque<CursorImageId>,
    /// The picture the client is drawing, if any.
    showing: Option<CursorImageId>,
    /// Whether the client has been told this host cannot track the pointer.
    said_untracked: bool,
    /// Whether a picture too large for a control message has been complained
    /// about, which happens once and not once a frame.
    said_refused: bool,
}

impl CursorFeed {
    /// A feed for an output laid out at this size.
    ///
    /// The laid-out size, not the captured one: every figure the compositor
    /// reports about the pointer is in these pixels. See
    /// [`tachyx_media::CaptureFormat::logical`].
    ///
    /// `None` for an output wider or taller than the wire can express, which is
    /// more than 65535 pixels on a side.
    #[must_use]
    pub fn new(logical: tachyx_media::Size) -> Option<Self> {
        let screen = Size::new(
            u16::try_from(logical.width()).ok()?,
            u16::try_from(logical.height()).ok()?,
        )?;
        Some(Self {
            screen,
            reported: None,
            sent: VecDeque::with_capacity(CursorImage::REMEMBERED),
            showing: None,
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

        let image = pointer
            .image
            .as_deref()
            .and_then(|picture| self.picture_of(picture));
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

    /// How the client is told which picture to draw, or `None` when it is
    /// already drawing that one.
    ///
    /// A shape the client has been sent travels as its name. That is the whole
    /// of the saving: a pointer crossing in and out of a text field flips
    /// between two shapes it already has, and repeating either of them puts
    /// tens of kilobytes on the stream the user's next click is waiting on.
    fn picture_of(&mut self, picture: &tachyx_media::CursorImage) -> Option<CursorPicture> {
        if self.showing == Some(picture.id) {
            return None;
        }
        if self.sent.contains(&picture.id) {
            self.showing = Some(picture.id);
            return Some(CursorPicture::Known(picture.id));
        }
        let encoded = self.encode(picture)?;
        self.sent.push_back(picture.id);
        while self.sent.len() > CursorImage::REMEMBERED {
            self.sent.pop_front();
        }
        self.showing = Some(picture.id);
        Some(CursorPicture::Whole(encoded))
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
            .and_then(|(width, height)| {
                CursorImage::new(
                    picture.id,
                    width,
                    height,
                    picture.scale,
                    picture.pixels.clone(),
                )
            });
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

    /// The output the host was measured on: 2880x1800 captured, laid out at a
    /// scale of 1.2.
    fn logical() -> tachyx_media::Size {
        tachyx_media::Size::new(2400, 1500).expect("a size")
    }

    fn feed() -> CursorFeed {
        CursorFeed::new(logical()).expect("a screen the wire can express")
    }

    fn arrow(id: u32, edge: u32, fill: u8) -> Arc<tachyx_media::CursorImage> {
        Arc::new(tachyx_media::CursorImage {
            id: CursorImageId(id),
            width: edge,
            height: edge,
            scale: std::num::NonZeroU8::new(2).expect("nonzero"),
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

    /// The picture a report carries whole, or a panic naming what it carried
    /// instead.
    fn whole(report: &CursorReport) -> &CursorImage {
        match report.image.as_ref() {
            Some(CursorPicture::Whole(image)) => image,
            other => panic!("expected a picture, got {other:?}"),
        }
    }

    #[test]
    fn the_first_report_carries_the_picture() {
        let mut feed = feed();
        let report = tracked(feed.poll(&shown(Some((1436, 710)), Some(arrow(0, 48, 0x7f)))));
        assert_eq!(
            report.screen,
            Size::new(2400, 1500).expect("a size"),
            "the laid-out size, not the 2880x1800 the frames are in"
        );
        assert_eq!(
            report.at,
            Some(CursorPoint {
                x: 1436,
                y: 710,
                hotspot_x: 5,
                hotspot_y: 1,
            })
        );
        let image = whole(&report);
        assert_eq!(
            image.logical(),
            (24.0, 24.0),
            "48 rows at a scale of 2 covers 24 of the 2400 the screen is wide"
        );
    }

    #[test]
    fn a_move_carries_no_picture() {
        let mut feed = feed();
        let picture = arrow(0, 48, 0x7f);
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
        let still = shown(Some((10, 10)), Some(arrow(0, 48, 0x7f)));
        assert!(feed.poll(&still).is_some());
        assert_eq!(feed.poll(&still), None);
        assert_eq!(feed.poll(&still), None);
    }

    #[test]
    fn a_new_picture_travels_when_the_pointer_crosses_a_widget() {
        let mut feed = feed();
        feed.poll(&shown(Some((10, 10)), Some(arrow(0, 48, 0x7f))));

        let mut beam = shown(Some((10, 10)), Some(arrow(1, 48, 0x22)));
        beam.serial = 2;
        let report = tracked(feed.poll(&beam));
        assert_eq!(
            whole(&report).pixels(),
            Some(&vec![0x22u8; 48 * 48 * 4][..])
        );
    }

    #[test]
    fn a_picture_the_pointer_comes_back_to_travels_as_its_name() {
        let mut feed = feed();
        let pointing = arrow(0, 48, 0x7f);
        feed.poll(&shown(Some((10, 10)), Some(Arc::clone(&pointing))));

        let mut beam = shown(Some((10, 10)), Some(arrow(1, 48, 0x22)));
        beam.serial = 2;
        assert!(tracked(feed.poll(&beam)).image.is_some());

        // Out of the text field again. Capture hands back the shape the arrow
        // already had, and the client still holds it, so the rows must not go
        // out a second time: they are 9 kB in front of the user's next click.
        let mut back = shown(Some((10, 10)), Some(pointing));
        back.serial = 3;
        assert_eq!(
            tracked(feed.poll(&back)).image,
            Some(CursorPicture::Known(CursorImageId(0)))
        );
    }

    #[test]
    fn a_picture_the_client_has_dropped_is_sent_again() {
        let mut feed = feed();
        // One shape more than either end keeps, so the first is gone from both.
        for id in 0..=u32::try_from(CursorImage::REMEMBERED).expect("a small bound") {
            let mut cursor = shown(Some((10, 10)), Some(arrow(id, 48, 0x7f)));
            cursor.serial = u64::from(id) + 1;
            assert!(matches!(
                tracked(feed.poll(&cursor)).image,
                Some(CursorPicture::Whole(_))
            ));
        }
        let mut first = shown(Some((10, 10)), Some(arrow(0, 48, 0x7f)));
        first.serial = 999;
        assert!(
            matches!(
                tracked(feed.poll(&first)).image,
                Some(CursorPicture::Whole(_))
            ),
            "naming a picture the client has dropped would leave the wrong pointer drawn"
        );
    }

    #[test]
    fn a_pointer_off_the_captured_screen_is_reported_as_gone() {
        let mut feed = feed();
        feed.poll(&shown(Some((10, 10)), Some(arrow(0, 48, 0x7f))));

        let mut left = shown(None, Some(arrow(0, 48, 0x7f)));
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
        let report = tracked(feed.poll(&shown(Some((10, 10)), Some(arrow(0, 256, 0x7f)))));
        assert_eq!(report.at.map(|at| (at.x, at.y)), Some((10, 10)));
        assert_eq!(report.image, None);

        // The picture was never sent, so a later position must still be able to
        // carry one that does fit.
        let mut smaller = shown(Some((11, 11)), Some(arrow(1, 48, 0x7f)));
        smaller.serial = 2;
        assert!(tracked(feed.poll(&smaller)).image.is_some());
    }
}
