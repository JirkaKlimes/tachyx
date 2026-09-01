//! The wire's input types, described so that Dart gets the same ones.
//!
//! Each declaration below mirrors a type in [`tachyx_proto::input`]. The
//! generated Dart classes are the protocol's own types rather than a second set
//! that has to be kept in step, so a change to the protocol breaks this file at
//! compile time instead of breaking the phone at run time.

use flutter_rust_bridge::frb;
pub use tachyx_proto::input::{HidUsage, InputEvent, PointerButton, Press};

/// Mirror of [`Press`].
#[frb(mirror(Press))]
#[derive(Debug)]
pub enum PressMirror {
    /// Now held.
    Down,
    /// Now released.
    Up,
}

/// Mirror of [`PointerButton`].
#[frb(mirror(PointerButton))]
#[derive(Debug)]
pub enum PointerButtonMirror {
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

/// Mirror of [`HidUsage`].
#[frb(mirror(HidUsage))]
#[derive(Debug)]
pub struct HidUsageMirror {
    /// The usage page. Android reports a value outside any HID page when an
    /// input method intercepts a key, so the client checks this before sending.
    pub page: u16,
    /// The usage within the page.
    pub id: u16,
}

/// Mirror of [`InputEvent`].
#[frb(mirror(InputEvent))]
#[derive(Debug)]
pub enum InputEventMirror {
    /// The pointer moved by this much, in pixels of the streamed image.
    PointerMotion {
        /// Horizontal displacement.
        dx: f32,
        /// Vertical displacement.
        dy: f32,
    },
    /// The pointer moved to this position, as a fraction of the streamed image.
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
    /// The fingers left the touch surface. Without it a flung surface on the
    /// host never settles.
    FingerScrollEnd,
    /// A key changed state. Genuine transitions only: the host repeats a held
    /// key itself.
    Key {
        /// Which key.
        usage: HidUsage,
        /// Its new state.
        press: Press,
    },
    /// Text entered by some means that is not a key press. Insertion only.
    Text(String),
}
