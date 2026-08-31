//! Input events travelling from a client to the host.
//!
//! Keys are carried as USB HID usage codes rather than characters, so a client
//! with a Czech layout and a host with a US layout still agree on which key the
//! user pressed. The host owns the layout.
//!
//! Text is carried separately, because an on-screen keyboard on a phone produces
//! characters and no HID usage code corresponds to most of what it can produce.

use serde::{Deserialize, Serialize};

/// Whether a key or button went down or came up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Press {
    /// The key or button is now held.
    Down,
    /// The key or button was released.
    Up,
}

/// A pointer button.
///
/// The set a remote desktop needs. Higher buttons exist in evdev but no client
/// we ship can produce them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerButton {
    /// Primary button.
    Left,
    /// Context-menu button.
    Right,
    /// Wheel button.
    Middle,
    /// Navigate back.
    Back,
    /// Navigate forward.
    Forward,
}

/// What produced a scroll, which decides whether the host reports it as a
/// discrete wheel detent or as smooth finger motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollSource {
    /// A mouse wheel, which moves in detents.
    Wheel,
    /// A touch surface, which moves continuously and can be flung.
    Finger,
}

/// A USB HID usage code from the keyboard usage page.
///
/// The host resolves this to a Linux evdev keycode. Usages the host cannot
/// resolve are dropped, not guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HidUsage(pub u16);

/// One thing the user did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Pointer moved by this much, in host pixels. This is what a touch screen
    /// used as a trackpad produces, and what a captured mouse produces.
    PointerMotion {
        /// Horizontal displacement.
        dx: f32,
        /// Vertical displacement.
        dy: f32,
    },
    /// Pointer moved to this position, as a fraction of the streamed image in
    /// each axis. A desktop client whose real mouse hovers over the video knows
    /// where the user is pointing and should say so directly.
    PointerMotionAbsolute {
        /// Horizontal position, `0.0` at the left edge and `1.0` at the right.
        x: f32,
        /// Vertical position, `0.0` at the top edge and `1.0` at the bottom.
        y: f32,
    },
    /// A pointer button changed state.
    PointerButton {
        /// Which button.
        button: PointerButton,
        /// Its new state.
        press: Press,
    },
    /// The surface under the pointer was scrolled, in host pixels.
    Scroll {
        /// Horizontal scroll distance, positive to the right.
        dx: f32,
        /// Vertical scroll distance, positive downwards.
        dy: f32,
        /// What produced it.
        source: ScrollSource,
    },
    /// A key changed state.
    Key {
        /// Which key, as a USB HID usage code.
        usage: HidUsage,
        /// Its new state.
        press: Press,
    },
    /// Text the user entered by some means that is not a key press, such as a
    /// phone's on-screen keyboard or a dictation engine. The host synthesises
    /// whatever key presses are needed to produce it.
    Text(String),
}
