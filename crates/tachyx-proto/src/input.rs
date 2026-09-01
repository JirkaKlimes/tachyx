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
    /// Now held.
    Down,
    /// Now released.
    Up,
}

/// A pointer button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerButton {
    /// The left physical button, whichever hand the user has configured.
    Left,
    /// The right physical button.
    Right,
    /// The wheel button.
    Middle,
    /// The button a mouse dedicates to navigating back.
    Back,
    /// The button a mouse dedicates to navigating forward.
    Forward,
}

/// A USB HID usage: which page it belongs to, and which usage within that page.
///
/// Flutter reports the two packed into one integer. They are separate here
/// because every use site needs one or the other, never the pair, and because a
/// page is what decides whether the host can act on the usage at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HidUsage {
    /// The usage page. Android reports a value outside any HID page when an input
    /// method intercepts a key, so this is checked before the usage is trusted.
    pub page: u16,
    /// The usage within the page.
    pub id: u16,
}

impl HidUsage {
    /// The keyboard page, the only one the host injects from.
    pub const KEYBOARD_PAGE: u16 = 0x0007;
}

/// One thing the user did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// The pointer moved by this much, measured in pixels of the streamed image.
    ///
    /// The host scales by whatever ratio its capture bears to the resolution it
    /// is currently encoding, so the pointer keeps the same feel when congestion
    /// steps the resolution down. A client cannot express this in the host's own
    /// pixels, because it is never told what they are.
    PointerMotion {
        /// Horizontal displacement.
        dx: f32,
        /// Vertical displacement.
        dy: f32,
    },
    /// The pointer moved to this position, as a fraction of the streamed image.
    ///
    /// A desktop client whose real mouse hovers over the video knows where the
    /// user is pointing and says so, rather than accumulating deltas.
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
    /// A wheel turned, in pixels of the streamed image.
    ///
    /// Separate from finger scrolling because a wheel moves in detents and has no
    /// end: there is no moment at which the user stops touching it.
    Wheel {
        /// Horizontal distance, positive to the right.
        dx: f32,
        /// Vertical distance, positive downwards.
        dy: f32,
    },
    /// Fingers dragged across a touch surface, in pixels of the streamed image.
    FingerScroll {
        /// Horizontal distance, positive to the right.
        dx: f32,
        /// Vertical distance, positive downwards.
        dy: f32,
    },
    /// The fingers left the touch surface.
    ///
    /// Applications need this to start their own kinetic scrolling and, more
    /// importantly, to stop it. Without it a flung surface never settles.
    FingerScrollEnd,
    /// A key changed state.
    ///
    /// A client sends only genuine transitions. The host repeats a held key
    /// itself, so forwarding a platform's own repeat events would double it.
    Key {
        /// Which key.
        usage: HidUsage,
        /// Its new state.
        press: Press,
    },
    /// Text the user entered by some means that is not a key press, such as a
    /// phone's on-screen keyboard.
    ///
    /// Insertion only. Deleting and entering a newline have real HID usages, so a
    /// client sends those as [`InputEvent::Key`] and they pass through the host's
    /// own layout untouched.
    Text(String),
}
