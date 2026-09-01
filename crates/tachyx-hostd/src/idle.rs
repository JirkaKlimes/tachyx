//! Keeping the screen awake while someone is watching it.
//!
//! The compositor stops producing frames when the display blanks, so an idle
//! timeout freezes the remote picture. Worse, the remote user cannot recover
//! from it: their input arrives at a blanked compositor, so the one person who
//! wants the machine awake is the one person who cannot wake it.
//!
//! Inhibiting is preferable to noticing and reacting, because a reaction always
//! lets one blanking cycle through first.

use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};

/// Why the screen could not be kept awake.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The compositor could not be reached.
    #[error("cannot connect to the wayland compositor")]
    Connect(#[from] wayland_client::ConnectError),
    /// The connection failed.
    #[error("wayland connection lost")]
    Dispatch(#[from] wayland_client::DispatchError),
    /// The compositor does not offer idle inhibition.
    #[error("compositor does not offer {0}")]
    Missing(&'static str),
}

/// Holds the screen awake for as long as it exists.
///
/// The surface behind the inhibitor is never committed and never given a role.
/// A compositor cannot decide whether such a surface is visible, and this one
/// resolves that by treating it as visible, which is what makes an inhibitor
/// usable from a daemon that has no window.
#[derive(Debug)]
pub struct Inhibitor {
    connection: Connection,
    held: ZwpIdleInhibitorV1,
    surface: wl_surface::WlSurface,
}

impl Inhibitor {
    /// Suppresses blanking until the returned value is dropped.
    ///
    /// # Errors
    /// If the compositor cannot be reached or does not offer idle inhibition.
    pub fn new() -> Result<Self, Error> {
        let connection = Connection::connect_to_env()?;
        let mut queue = connection.new_event_queue();
        let handle = queue.handle();
        connection.display().get_registry(&handle, ());

        let mut state = State::default();
        queue.roundtrip(&mut state)?;

        let compositor = state
            .compositor
            .clone()
            .ok_or(Error::Missing("wl_compositor"))?;
        let manager = state
            .manager
            .clone()
            .ok_or(Error::Missing("zwp_idle_inhibit_manager_v1"))?;

        let surface = compositor.create_surface(&handle, ());
        let held = manager.create_inhibitor(&surface, &handle, ());
        queue.roundtrip(&mut state)?;

        Ok(Self {
            connection,
            held,
            surface,
        })
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        self.held.destroy();
        self.surface.destroy();
        if let Err(error) = self.connection.flush() {
            tracing::warn!(%error, "cannot release the idle inhibitor");
        }
    }
}

#[derive(Debug, Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    manager: Option<ZwpIdleInhibitManagerV1>,
}

delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore ZwpIdleInhibitManagerV1);
delegate_noop!(State: ignore ZwpIdleInhibitorV1);

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
            "wl_compositor" => {
                state.compositor = Some(registry.bind(name, version.min(6), handle, ()));
            }
            "zwp_idle_inhibit_manager_v1" => {
                state.manager = Some(registry.bind(name, 1, handle, ()));
            }
            _ => {}
        }
    }
}
