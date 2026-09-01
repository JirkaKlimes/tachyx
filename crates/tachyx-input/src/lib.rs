//! Pointer and keyboard injection into a wlroots compositor.
//!
//! Injection uses the virtual pointer and virtual keyboard protocols directly.
//! The portal's remote desktop interface, which the architecture names as the
//! default path, does not exist on Hyprland.
//!
//! An [`Injector`] owns a Wayland connection and is driven from a single thread.
//! It is deliberately synchronous: injection is a trickle of tiny messages, and
//! an async runtime behind it would buy nothing.

mod keymap;

use std::os::fd::AsFd as _;
use std::time::Instant;

use keycode::{KeyMap, KeyMapping};
use keymap::TextKeymap;
use tachyx_proto::input::{HidUsage, InputEvent, PointerButton, Press};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

/// Why injection could not be set up or carried out.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The compositor could not be reached.
    #[error("cannot connect to the wayland compositor")]
    Connect(#[from] wayland_client::ConnectError),
    /// The connection to the compositor failed.
    #[error("wayland connection lost")]
    Dispatch(#[from] wayland_client::DispatchError),
    /// A request to the compositor could not be delivered.
    #[error("wayland request failed")]
    Wayland(#[from] wayland_client::backend::WaylandError),
    /// The compositor does not offer a protocol that injection requires.
    #[error("compositor does not offer {0}, which input injection requires")]
    Missing(&'static str),
    /// The compositor announced no output by the requested name.
    #[error("no output named {0}")]
    NoSuchOutput(String),
    /// The compositor's own keymap could not be read back from it.
    #[error("cannot read the compositor's keymap")]
    Keymap(#[source] std::io::Error),
}

/// A monitor's place in the layout, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// Linux button codes, from `input-event-codes.h`.
const fn button_code(button: PointerButton) -> u32 {
    match button {
        PointerButton::Left => 0x110,
        PointerButton::Right => 0x111,
        PointerButton::Middle => 0x112,
        PointerButton::Forward => 0x115,
        PointerButton::Back => 0x116,
    }
}

/// XKB modifier masks, from the standard `complete` set the keymaps include.
mod mask {
    pub(super) const SHIFT: u32 = 1;
    pub(super) const LOCK: u32 = 1 << 1;
    pub(super) const CONTROL: u32 = 1 << 2;
    pub(super) const ALT: u32 = 1 << 3;
    pub(super) const NUM: u32 = 1 << 4;
    pub(super) const SUPER: u32 = 1 << 6;
    pub(super) const ALT_GR: u32 = 1 << 7;
}

/// The modifier mask a key contributes while it is held.
const fn held_modifier(usage: u16) -> u32 {
    match usage {
        0xE0 | 0xE4 => mask::CONTROL,
        0xE1 | 0xE5 => mask::SHIFT,
        0xE2 => mask::ALT,
        0xE6 => mask::ALT_GR,
        0xE3 | 0xE7 => mask::SUPER,
        _ => 0,
    }
}

/// The modifier mask a key toggles when pressed.
const fn locking_modifier(usage: u16) -> u32 {
    match usage {
        0x39 => mask::LOCK,
        0x53 => mask::NUM,
        _ => 0,
    }
}

/// Injects pointer and keyboard events into the compositor.
///
/// Two virtual keyboards exist rather than one. Physical keys must be resolved
/// through the host's own layout, which is the entire reason keys travel as HID
/// usage codes. Text from an on-screen keyboard must bypass layout, because the
/// character it produced has no relationship to any key. One keymap cannot serve
/// both without rewriting the layout that the focused application is using.
#[derive(Debug)]
pub struct Injector {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    pointer: ZwlrVirtualPointerV1,
    layout_keyboard: ZwpVirtualKeyboardV1,
    text_keyboard: ZwpVirtualKeyboardV1,
    text_keymap: TextKeymap,
    output: Rect,
    layout: Rect,
    held: u32,
    locked: u32,
    epoch: Instant,
}

impl Injector {
    /// Connects to the compositor and prepares injection for one output.
    ///
    /// `output` names the monitor whose surface absolute positions are measured
    /// against, such as `eDP-1`.
    ///
    /// # Errors
    /// If the compositor cannot be reached, does not offer the virtual pointer or
    /// virtual keyboard protocols, has no output by that name, or will not hand
    /// back the keymap it is using.
    pub fn new(output: &str) -> Result<Self, Error> {
        let connection = Connection::connect_to_env()?;
        let mut queue = connection.new_event_queue();
        let handle = queue.handle();
        connection.display().get_registry(&handle, ());

        let mut state = State::default();
        queue.roundtrip(&mut state)?;

        let xdg_outputs = state
            .xdg_output_manager
            .clone()
            .ok_or(Error::Missing("zxdg_output_manager_v1"))?;
        for (index, output) in state.outputs.clone().iter().enumerate() {
            xdg_outputs.get_xdg_output(output, &handle, index);
        }
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;

        let chosen = state
            .geometry
            .iter()
            .find(|(name, _)| name == output)
            .map(|(_, rect)| *rect)
            .ok_or_else(|| Error::NoSuchOutput(output.to_owned()))?;

        let pointer = state
            .pointer_manager
            .clone()
            .ok_or(Error::Missing("zwlr_virtual_pointer_manager_v1"))?
            .create_virtual_pointer(state.seat.as_ref(), &handle, ());
        let keyboards = state
            .keyboard_manager
            .clone()
            .ok_or(Error::Missing("zwp_virtual_keyboard_manager_v1"))?;
        let seat = state.seat.clone().ok_or(Error::Missing("wl_seat"))?;
        let layout_keyboard = keyboards.create_virtual_keyboard(&seat, &handle, ());
        let text_keyboard = keyboards.create_virtual_keyboard(&seat, &handle, ());

        let host_keymap = state.host_keymap.clone().ok_or_else(|| {
            Error::Keymap(std::io::Error::other(
                "the compositor announced no keyboard keymap",
            ))
        })?;
        upload_keymap(&layout_keyboard, &host_keymap).map_err(Error::Keymap)?;
        connection.roundtrip()?;

        Ok(Self {
            connection,
            queue,
            state,
            pointer,
            layout_keyboard,
            text_keyboard,
            text_keymap: TextKeymap::new(),
            output: chosen,
            layout: Rect::default(),
            held: 0,
            locked: 0,
            epoch: Instant::now(),
        })
    }

    /// Applies a batch of events and commits them as one group.
    ///
    /// Events are batched by the client on a short tick, so a burst of pointer
    /// motion costs one commit rather than one per sample.
    ///
    /// # Errors
    /// If the connection to the compositor fails.
    pub fn apply(&mut self, events: &[InputEvent]) -> Result<(), Error> {
        self.layout = self.state.layout_bounds();
        let mut touched_pointer = false;
        for event in events {
            touched_pointer |= self.apply_one(event);
        }
        if touched_pointer {
            self.pointer.frame();
        }
        self.connection.flush()?;
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }

    fn apply_one(&mut self, event: &InputEvent) -> bool {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a session outlives its own millisecond counter only after 49 days"
        )]
        let time = self.epoch.elapsed().as_millis() as u32;
        match *event {
            InputEvent::PointerMotion { dx, dy } => {
                self.pointer.motion(time, f64::from(dx), f64::from(dy));
                true
            }
            InputEvent::PointerMotionAbsolute { x, y } => {
                let (px, py) = locate(self.layout, self.output, x, y);
                #[expect(
                    clippy::cast_sign_loss,
                    reason = "the layout bounds are positive by construction"
                )]
                self.pointer.motion_absolute(
                    time,
                    px,
                    py,
                    self.layout.width.max(1) as u32,
                    self.layout.height.max(1) as u32,
                );
                true
            }
            InputEvent::PointerButton { button, press } => {
                self.pointer
                    .button(time, button_code(button), press_state(press));
                true
            }
            InputEvent::Wheel { dx, dy } => {
                self.scroll(time, dx, dy, wl_pointer::AxisSource::Wheel);
                true
            }
            InputEvent::FingerScroll { dx, dy } => {
                self.scroll(time, dx, dy, wl_pointer::AxisSource::Finger);
                true
            }
            InputEvent::FingerScrollEnd => {
                self.pointer.axis_source(wl_pointer::AxisSource::Finger);
                self.pointer
                    .axis_stop(time, wl_pointer::Axis::VerticalScroll);
                self.pointer
                    .axis_stop(time, wl_pointer::Axis::HorizontalScroll);
                true
            }
            InputEvent::Key { usage, press } => {
                self.key(time, usage, press);
                false
            }
            InputEvent::Text(ref text) => {
                self.text(time, text);
                false
            }
        }
    }

    fn scroll(&self, time: u32, dx: f32, dy: f32, source: wl_pointer::AxisSource) {
        self.pointer.axis_source(source);
        for (axis, delta) in [
            (wl_pointer::Axis::HorizontalScroll, dx),
            (wl_pointer::Axis::VerticalScroll, dy),
        ] {
            if delta == 0.0 {
                continue;
            }
            self.pointer.axis(time, axis, f64::from(delta));
            if source == wl_pointer::AxisSource::Wheel {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "a detent count is small by construction"
                )]
                self.pointer.axis_discrete(
                    time,
                    axis,
                    f64::from(delta),
                    (delta / WHEEL_DETENT).trunc() as i32,
                );
            }
        }
    }

    fn key(&mut self, time: u32, usage: HidUsage, press: Press) {
        if usage.page != HidUsage::KEYBOARD_PAGE {
            tracing::debug!(
                page = usage.page,
                "dropping a key outside the keyboard page"
            );
            return;
        }
        let Ok(mapping) = KeyMap::from_key_mapping(KeyMapping::Usb(usage.id)) else {
            tracing::debug!(id = usage.id, "dropping a key with no evdev equivalent");
            return;
        };

        let before = (self.held, self.locked);
        let held = held_modifier(usage.id);
        match press {
            Press::Down => {
                self.held |= held;
                self.locked ^= locking_modifier(usage.id);
            }
            Press::Up => self.held &= !held,
        }
        // The compositor does not derive modifier state from injected keys, so it
        // is stated rather than implied.
        if (self.held, self.locked) != before {
            self.layout_keyboard.modifiers(self.held, 0, self.locked, 0);
        }
        self.layout_keyboard
            .key(time, mapping.evdev.into(), press_value(press));
    }

    fn text(&mut self, time: u32, text: &str) {
        if let Some(keymap) = self.text_keymap.admit(text) {
            if let Err(error) = upload_keymap(&self.text_keyboard, keymap.as_bytes()) {
                tracing::warn!(%error, "cannot upload the text keymap");
                return;
            }
            // The compositor must have compiled the keymap before a key that
            // depends on it is sent.
            if let Err(error) = self.connection.roundtrip() {
                tracing::warn!(%error, "wayland round trip failed uploading the text keymap");
                return;
            }
        }
        // A stale modifier would turn a plain character into something else.
        self.text_keyboard.modifiers(0, 0, 0, 0);
        for character in text.chars() {
            let Some(code) = self.text_keymap.keycode(character) else {
                tracing::warn!(%character, "no key available for character");
                continue;
            };
            self.text_keyboard.key(time, code, 1);
            self.text_keyboard.key(time, code, 0);
        }
    }
}

/// Distance in scroll units that makes up one wheel detent.
const WHEEL_DETENT: f32 = 15.0;

const fn press_value(press: Press) -> u32 {
    match press {
        Press::Down => 1,
        Press::Up => 0,
    }
}

const fn press_state(press: Press) -> wl_pointer::ButtonState {
    match press {
        Press::Down => wl_pointer::ButtonState::Pressed,
        Press::Up => wl_pointer::ButtonState::Released,
    }
}

/// Maps a position inside one output onto the whole layout.
fn locate(layout: Rect, output: Rect, x: f32, y: f32) -> (u32, u32) {
    #[expect(
        clippy::cast_precision_loss,
        reason = "desktop coordinates are far below the exact integer range of a float"
    )]
    let (lx, ly) = (
        (output.x - layout.x) as f32 + x.clamp(0.0, 1.0) * output.width as f32,
        (output.y - layout.y) as f32 + y.clamp(0.0, 1.0) * output.height as f32,
    );
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "both values are clamped inside the layout before conversion"
    )]
    (lx.max(0.0) as u32, ly.max(0.0) as u32)
}

/// Writes a keymap into a memory file and hands it to the compositor.
fn upload_keymap(keyboard: &ZwpVirtualKeyboardV1, keymap: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let file = rustix::fs::memfd_create("tachyx-keymap", rustix::fs::MemfdFlags::CLOEXEC)?;
    let mut file = std::fs::File::from(file);
    file.write_all(keymap)?;
    let mut length = keymap.len();
    // The compositor reads the keymap as a C string, and the length it is given
    // must count the terminator.
    if keymap.last() != Some(&0) {
        file.write_all(&[0])?;
        length += 1;
    }
    file.flush()?;
    keyboard.keymap(
        wl_keyboard::KeymapFormat::XkbV1.into(),
        file.as_fd(),
        u32::try_from(length).map_err(|_| std::io::Error::other("keymap is implausibly large"))?,
    );
    Ok(())
}

#[derive(Debug, Default)]
struct State {
    outputs: Vec<wl_output::WlOutput>,
    geometry: Vec<(String, Rect)>,
    seat: Option<wl_seat::WlSeat>,
    host_keymap: Option<Vec<u8>>,
    xdg_output_manager: Option<ZxdgOutputManagerV1>,
    pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
}

impl State {
    /// The bounding box of every output together.
    fn layout_bounds(&self) -> Rect {
        let mut bounds: Option<Rect> = None;
        for (_, rect) in &self.geometry {
            bounds = Some(bounds.map_or(*rect, |acc: Rect| {
                let x = acc.x.min(rect.x);
                let y = acc.y.min(rect.y);
                Rect {
                    x,
                    y,
                    width: (acc.x + acc.width).max(rect.x + rect.width) - x,
                    height: (acc.y + acc.height).max(rect.y + rect.height) - y,
                }
            }));
        }
        bounds.unwrap_or_default()
    }
}

delegate_noop!(State: ignore wl_output::WlOutput);
delegate_noop!(State: ignore ZxdgOutputManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardV1);

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_output" => state
                .outputs
                .push(registry.bind(name, version.min(4), handle, ())),
            "wl_seat" => {
                state.seat = Some(registry.bind(name, version.min(7), handle, ()));
            }
            "zxdg_output_manager_v1" => {
                state.xdg_output_manager = Some(registry.bind(name, version.min(3), handle, ()));
            }
            "zwlr_virtual_pointer_manager_v1" => {
                state.pointer_manager = Some(registry.bind(name, version.min(2), handle, ()));
            }
            "zwp_virtual_keyboard_manager_v1" => {
                state.keyboard_manager = Some(registry.bind(name, 1, handle, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.geometry.len() <= *index {
            state
                .geometry
                .resize_with(*index + 1, || (String::new(), Rect::default()));
        }
        let (name, rect) = &mut state.geometry[*index];
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                rect.x = x;
                rect.y = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                rect.width = width;
                rect.height = height;
            }
            zxdg_output_v1::Event::Name { name: announced } => *name = announced,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        (): &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: wayland_client::WEnum::Value(capabilities),
        } = event
            && capabilities.contains(wl_seat::Capability::Keyboard)
        {
            // Asking for the keyboard is how the host's own layout is obtained:
            // the compositor sends its keymap immediately, without waiting for
            // this client to hold focus, and it never will.
            seat.get_keyboard(handle, ());
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let wl_keyboard::Event::Keymap { fd, size, .. } = event else {
            return;
        };
        let mut keymap = vec![0u8; size as usize];
        let mut filled = 0;
        while filled < keymap.len() {
            match rustix::io::pread(&fd, &mut keymap[filled..], filled as u64) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => {
                    tracing::warn!(%error, "cannot read the compositor keymap");
                    return;
                }
            }
        }
        keymap.truncate(filled);
        state.host_keymap = Some(keymap);
    }
}
