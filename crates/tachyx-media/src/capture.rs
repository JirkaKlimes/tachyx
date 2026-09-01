//! Capturing one output through `ext-image-copy-capture-v1`.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use gbm::{BufferObject, BufferObjectFlags, Format, Modifier};
use rustix::event::{PollFd, PollFlags, poll};
use tachyx_proto::media::Timestamp;
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{wl_buffer, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm};
use wayland_client::protocol::{wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_cursor_session_v1::{self, ExtImageCopyCaptureCursorSessionV1},
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use crate::slot::Slot;
use crate::{Error, Framerate, Size};

/// The modifier the protocol uses for "any layout the allocator likes", which
/// no allocator can be asked to produce.
const MODIFIER_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Buffers in the ring.
///
/// Three is the minimum, not a margin. Measured with two: a buffer the encoder
/// still holds when the next one becomes ready leaves nothing to arm with and
/// nothing to displace, and `publish` reaches an unarmable state within a
/// second of starting.
const RING: usize = 3;

/// Where one plane of a shared buffer starts and how wide its rows are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plane {
    /// Bytes from the start of the descriptor to this plane.
    pub offset: u32,
    /// Bytes per row.
    pub stride: u32,
}

/// What the compositor and the allocator settled on, and what the encoder must
/// therefore be told to import.
#[derive(Debug, Clone)]
pub struct CaptureFormat {
    /// Name of the captured output, such as `eDP-1`.
    pub output: String,
    /// Size of every frame, in pixels.
    pub size: Size,
    /// The output's refresh rate, which is also the rate the encoder's bit
    /// budget must be divided by.
    pub refresh: Framerate,
    /// DRM `fourcc` of the pixel format.
    pub fourcc: u32,
    /// DRM modifier describing the memory layout.
    pub modifier: u64,
    /// The render node the buffers were allocated on.
    pub render_node: PathBuf,
    /// Layout of every plane, shared by all buffers in the ring.
    pub planes: Vec<Plane>,
    /// Bytes in the shared descriptor.
    pub length: usize,
}

/// One captured picture, holding its buffer out of the ring until it is dropped.
#[derive(Debug)]
pub struct Frame {
    id: u64,
    captured: Timestamp,
    presented_nanos: u64,
    fd: OwnedFd,
    lease: Lease,
}

impl Frame {
    /// Position in the capture sequence, counted over the session.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// When the compositor presented the picture, relative to the session start.
    #[must_use]
    pub const fn captured(&self) -> Timestamp {
        self.captured
    }

    /// The compositor's presentation time, on `CLOCK_MONOTONIC`.
    #[must_use]
    pub const fn presented(&self) -> Duration {
        Duration::from_nanos(self.presented_nanos)
    }

    /// How long ago the picture was presented.
    ///
    /// This is the age of the content rather than of the frame. Hyprland
    /// timestamps the copy rather than the scan-out, so the figure is small:
    /// 0.04 ms median and 0.09 ms worst over 600 frames at the refresh rate.
    #[must_use]
    pub fn age(&self) -> Duration {
        Duration::from_nanos(monotonic_nanos().saturating_sub(self.presented_nanos))
    }

    pub(crate) fn into_parts(self) -> (OwnedFd, Lease) {
        (self.fd, self.lease)
    }
}

/// Keeps one buffer of the ring out of circulation, and returns it on drop.
#[derive(Debug)]
pub(crate) struct Lease {
    index: usize,
    returned: Arc<Mutex<Vec<usize>>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.returned
            .lock()
            .expect("the ring mutex is never held across a panic")
            .push(self.index);
    }
}

/// The consumer end of the depth-one slot between capture and encode.
#[derive(Debug)]
pub struct Frames {
    slot: Arc<Slot<Frame>>,
    format: CaptureFormat,
}

impl Frames {
    /// What every frame looks like.
    #[must_use]
    pub const fn format(&self) -> &CaptureFormat {
        &self.format
    }

    /// Waits for the newest frame, or returns `None` once capture has ended.
    ///
    /// Hold at most one frame at a time. A frame keeps a buffer out of the ring
    /// until it is dropped, and the ring has exactly enough buffers for one
    /// held frame, one armed request and one in the slot. Holding two leaves
    /// capture with nothing it may safely arm, which it reports by ending.
    #[must_use]
    pub fn next(&self) -> Option<Frame> {
        self.slot.take()
    }

    pub(crate) fn slot(&self) -> Arc<Slot<Frame>> {
        Arc::clone(&self.slot)
    }
}

/// What capture has done so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CaptureStats {
    /// Frames the compositor handed over.
    pub captured: u64,
    /// Frames replaced in the slot before the encoder took them.
    pub dropped: u64,
    /// Frames the compositor refused to fill.
    pub failed: u64,
    /// Times no buffer was free to arm at the moment one became ready.
    ///
    /// The ring recovers by publishing first, which displaces the pending frame
    /// and hands its buffer straight back, so this costs no vertical blank. It
    /// is the measurement that says whether the ring is the right size: a run
    /// that never reaches it has a spare buffer it does not need.
    pub starved: u64,
}

#[derive(Debug, Default)]
struct Counters {
    captured: AtomicU64,
    dropped: AtomicU64,
    failed: AtomicU64,
    starved: AtomicU64,
}

/// Where the pointer is and what it looks like.
///
/// The frame itself is captured without the cursor painted into it, so the
/// client draws it locally and pointer movement is not gated on a round trip.
#[derive(Debug, Clone, Default)]
pub struct Cursor {
    /// Whether the pointer is over the captured output.
    pub visible: bool,
    /// Position of the cursor's hotspot, in output pixels.
    pub position: (i32, i32),
    /// Where the hotspot sits inside the image.
    pub hotspot: (i32, i32),
    /// The pointer's picture, once one has been captured.
    pub image: Option<CursorImage>,
    /// Bumped whenever anything above changes.
    pub serial: u64,
}

/// A pointer image, as premultiplied ARGB rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height` little-endian ARGB pixels, row by row.
    pub pixels: Vec<u8>,
}

/// A running capture of one output.
///
/// Dropping this stops the capture thread and closes the slot, so a consumer
/// waiting in [`Frames::next`] is released.
#[derive(Debug)]
pub struct Capture {
    format: CaptureFormat,
    counters: Arc<Counters>,
    cursor: Arc<Mutex<Cursor>>,
    stop: OwnedFd,
    thread: Option<JoinHandle<Result<(), Error>>>,
}

impl Capture {
    /// Starts capturing the named output on a thread of its own.
    ///
    /// The thread owns the Wayland connection, the ring of shared buffers and
    /// the cursor session, and publishes into the slot behind the returned
    /// [`Frames`].
    ///
    /// # Errors
    /// If the compositor cannot be reached, offers no output by that name, does
    /// not implement the capture protocols, names a render node this host does
    /// not have, or shares no buffer layout with the encoder.
    pub fn start(output: &str) -> Result<(Self, Frames), Error> {
        gstreamer::init()?;
        let stop = rustix::event::eventfd(0, rustix::event::EventfdFlags::CLOEXEC)?;
        let wake = stop.try_clone()?;

        let slot = Arc::new(Slot::new());
        let counters = Arc::new(Counters::default());
        let cursor = Arc::new(Mutex::new(Cursor::default()));
        let (ready, started) = sync_channel(1);

        let thread = {
            let output = output.to_owned();
            let slot = Arc::clone(&slot);
            let counters = Arc::clone(&counters);
            let cursor = Arc::clone(&cursor);
            std::thread::Builder::new()
                .name("tachyx-capture".to_owned())
                .spawn(move || run(&output, wake, &slot, &counters, &cursor, &ready))?
        };

        match started.recv() {
            Ok(Ok(format)) => Ok((
                Self {
                    format: format.clone(),
                    counters,
                    cursor,
                    stop,
                    thread: Some(thread),
                },
                Frames { slot, format },
            )),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Gst("the capture thread died starting up".to_owned())),
        }
    }

    /// What every frame looks like.
    #[must_use]
    pub const fn format(&self) -> &CaptureFormat {
        &self.format
    }

    /// Counters since the session began.
    #[must_use]
    pub fn stats(&self) -> CaptureStats {
        CaptureStats {
            captured: self.counters.captured.load(Ordering::Relaxed),
            dropped: self.counters.dropped.load(Ordering::Relaxed),
            failed: self.counters.failed.load(Ordering::Relaxed),
            starved: self.counters.starved.load(Ordering::Relaxed),
        }
    }

    /// The pointer as it was last reported.
    #[must_use]
    pub fn cursor(&self) -> Cursor {
        // A poisoned lock means the capture thread panicked while updating the
        // cursor. What is behind it is still a valid cursor, so reading a
        // slightly stale position beats failing the call over it.
        self.cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Stops capture and reports how the thread ended.
    ///
    /// # Errors
    /// Whatever ended the capture thread, unless it was this call.
    pub fn stop(mut self) -> Result<(), Error> {
        let thread = self.thread.take();
        self.signal();
        match thread.map(JoinHandle::join) {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(Error::Gst("the capture thread panicked".to_owned())),
            None => Ok(()),
        }
    }

    fn signal(&self) {
        if let Err(error) = rustix::io::write(&self.stop, &1u64.to_ne_bytes()) {
            tracing::warn!(%error, "cannot wake the capture thread to stop it");
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.signal();
        if let Ok(Err(error)) = thread.join() {
            tracing::warn!(%error, "capture ended in an error");
        }
    }
}

fn monotonic_nanos() -> u64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    #[expect(
        clippy::cast_sign_loss,
        reason = "CLOCK_MONOTONIC counts up from an unspecified point in the past"
    )]
    {
        now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Screen,
    Cursor,
}

#[derive(Debug, Default)]
struct SessionState {
    size: Option<(u32, u32)>,
    device: Option<u64>,
    formats: Vec<(u32, Vec<u64>)>,
    shm_formats: Vec<wl_shm::Format>,
    done: bool,
    stopped: bool,
    ready: bool,
    failed: Option<String>,
    presented: Option<u64>,
}

impl SessionState {
    fn take_frame(&mut self) -> (bool, Option<String>, Option<u64>) {
        (
            std::mem::take(&mut self.ready),
            self.failed.take(),
            self.presented.take(),
        )
    }
}

#[derive(Debug, Default)]
struct State {
    outputs: Vec<wl_output::WlOutput>,
    names: HashMap<u32, String>,
    refresh: HashMap<u32, u32>,
    seat: Option<wl_seat::WlSeat>,
    shm: Option<wl_shm::WlShm>,
    sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    copies: Option<ExtImageCopyCaptureManagerV1>,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    screen: SessionState,
    cursor: SessionState,
    pointer: Cursor,
    pointer_changed: bool,
}

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
            "wl_seat" => state.seat = Some(registry.bind(name, version.min(7), handle, ())),
            "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), handle, ())),
            "ext_output_image_capture_source_manager_v1" => {
                state.sources = Some(registry.bind(name, version, handle, ()));
            }
            "ext_image_copy_capture_manager_v1" => {
                state.copies = Some(registry.bind(name, version, handle, ()));
            }
            "zwp_linux_dmabuf_v1" => {
                state.dmabuf = Some(registry.bind(name, version.min(4), handle, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = output.id().protocol_id();
        match event {
            wl_output::Event::Name { name } => {
                state.names.insert(id, name);
            }
            wl_output::Event::Mode { flags, refresh, .. }
                if flags
                    .into_result()
                    .is_ok_and(|f| f.contains(wl_output::Mode::Current)) =>
            {
                #[expect(
                    clippy::cast_sign_loss,
                    reason = "a mode line never reports a negative refresh"
                )]
                state.refresh.insert(id, refresh.max(0) as u32);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, Target> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        target: &Target,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event as E;
        let session = match target {
            Target::Screen => &mut state.screen,
            Target::Cursor => &mut state.cursor,
        };
        match event {
            E::BufferSize { width, height } => session.size = Some((width, height)),
            E::DmabufDevice { device } => session.device = read_u64(&device),
            E::DmabufFormat { format, modifiers } => {
                let modifiers = modifiers
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|bytes| u64::from_ne_bytes(*bytes))
                    .collect();
                session.formats.push((format, modifiers));
            }
            E::ShmFormat {
                format: WEnum::Value(format),
            } => session.shm_formats.push(format),
            E::Done => session.done = true,
            E::Stopped => session.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, Target> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        target: &Target,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event as E;
        let session = match target {
            Target::Screen => &mut state.screen,
            Target::Cursor => &mut state.cursor,
        };
        match event {
            E::PresentationTime {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let seconds = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                session.presented = Some(seconds * 1_000_000_000 + u64::from(tv_nsec));
            }
            E::Ready => session.ready = true,
            E::Failed { reason } => session.failed = Some(format!("{reason:?}")),
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureCursorSessionV1,
        event: ext_image_copy_capture_cursor_session_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_cursor_session_v1::Event as E;
        match event {
            E::Enter => state.pointer.visible = true,
            E::Leave => state.pointer.visible = false,
            E::Position { x, y } => state.pointer.position = (x, y),
            E::Hotspot { x, y } => state.pointer.hotspot = (x, y),
            _ => return,
        }
        state.pointer_changed = true;
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, zwp_linux_buffer_params_v1::Event::Failed) {
            tracing::error!("the compositor rejected a shared buffer");
        }
    }
}

delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_pointer::WlPointer);
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore ZwpLinuxDmabufV1);
delegate_noop!(State: ignore ExtImageCaptureSourceV1);
delegate_noop!(State: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);

fn read_u64(bytes: &[u8]) -> Option<u64> {
    bytes
        .first_chunk::<8>()
        .map(|word| u64::from_ne_bytes(*word))
}

/// Finds the `/dev/dri` entry the compositor named by device number.
///
/// The card index and the render node index are not related: on a machine with
/// two GPUs `card0` and `renderD128` routinely belong to different ones, and
/// allocating on the wrong node fails.
fn render_node(device: u64) -> Result<PathBuf, Error> {
    for entry in std::fs::read_dir("/dev/dri")? {
        let path = entry?.path();
        if std::fs::metadata(&path).is_ok_and(|meta| meta.rdev() == device) {
            return Ok(path);
        }
    }
    Err(Error::NoRenderNode(device))
}

/// The buffer ring, plus everything needed to keep it turning.
struct Pump {
    connection: Connection,
    queue: EventQueue<State>,
    handle: QueueHandle<State>,
    stop: OwnedFd,
    session: ExtImageCopyCaptureSessionV1,
    buffers: Vec<Slab>,
    armed: Option<(usize, ExtImageCopyCaptureFrameV1)>,
    free: Vec<usize>,
    returned: Arc<Mutex<Vec<usize>>>,
    slot: Arc<Slot<Frame>>,
    counters: Arc<Counters>,
    cursor: Arc<Mutex<Cursor>>,
    pointer: Option<PointerCapture>,
    size: Size,
    epoch: u64,
    next_id: u64,
}

/// One buffer of the ring: the allocation and the compositor's handle on it.
struct Slab {
    object: BufferObject<()>,
    buffer: wl_buffer::WlBuffer,
}

/// The cursor session and the shared memory its picture is copied into.
struct PointerCapture {
    session: ExtImageCopyCaptureSessionV1,
    buffer: wl_buffer::WlBuffer,
    storage: std::fs::File,
    size: (u32, u32),
    armed: Option<ExtImageCopyCaptureFrameV1>,
}

fn run(
    output: &str,
    stop: OwnedFd,
    slot: &Arc<Slot<Frame>>,
    counters: &Arc<Counters>,
    cursor: &Arc<Mutex<Cursor>>,
    ready: &SyncSender<Result<CaptureFormat, Error>>,
) -> Result<(), Error> {
    // Closing however this thread ends, a panic included, is what releases a
    // consumer waiting in `Frames::next`. Closing only on the way out of a
    // clean return would leave one blocked for ever on any other ending.
    let _closing = CloseOnDrop(slot);
    let mut state = State::default();
    let (mut pump, format) = match Pump::setup(output, stop, slot, counters, cursor, &mut state) {
        Ok(started) => started,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };
    if ready.send(Ok(format)).is_err() {
        return Ok(());
    }
    pump.run(&mut state)
}

/// Ends delivery when it goes out of scope, whatever ended the capture thread.
struct CloseOnDrop<'a>(&'a Slot<Frame>);

impl Drop for CloseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl Pump {
    fn setup(
        output: &str,
        stop: OwnedFd,
        slot: &Arc<Slot<Frame>>,
        counters: &Arc<Counters>,
        cursor: &Arc<Mutex<Cursor>>,
        state: &mut State,
    ) -> Result<(Self, CaptureFormat), Error> {
        let connection = Connection::connect_to_env()?;
        let mut queue = connection.new_event_queue();
        let handle = queue.handle();
        connection.display().get_registry(&handle, ());
        queue.roundtrip(state)?;
        queue.roundtrip(state)?;

        let (chosen, refresh) = state.find_output(output)?;
        let sources = state
            .sources
            .clone()
            .ok_or(Error::Missing("ext_output_image_capture_source_manager_v1"))?;
        let copies = state
            .copies
            .clone()
            .ok_or(Error::Missing("ext_image_copy_capture_manager_v1"))?;
        let dmabuf = state
            .dmabuf
            .clone()
            .ok_or(Error::Missing("zwp_linux_dmabuf_v1"))?;

        let source = sources.create_source(&chosen, &handle, ());
        let session = copies.create_session(
            &source,
            ext_image_copy_capture_manager_v1::Options::empty(),
            &handle,
            Target::Screen,
        );
        while !state.screen.done && !state.screen.stopped {
            queue.blocking_dispatch(state)?;
        }
        if state.screen.stopped {
            return Err(Error::SessionStopped);
        }

        let (width, height) = state.screen.size.ok_or(Error::SessionStopped)?;
        let size = Size::new(width, height).ok_or(Error::SessionStopped)?;
        let device = state.screen.device.ok_or(Error::NoSharedFormat)?;
        let node = render_node(device)?;
        let (fourcc, modifier) = choose_layout(&state.screen.formats)?;

        let gbm = gbm::Device::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&node)?,
        )?;
        let mut buffers = Vec::with_capacity(RING);
        for _ in 0..RING {
            buffers.push(Slab::allocate(
                &gbm, size, fourcc, modifier, &dmabuf, &handle,
            )?);
        }
        queue.roundtrip(state)?;
        let (planes, length) = buffers[0].layout()?;

        let format = CaptureFormat {
            output: output.to_owned(),
            size,
            refresh,
            fourcc: fourcc as u32,
            modifier: modifier.into(),
            render_node: node,
            planes,
            length,
        };

        let pointer = PointerCapture::setup(state, &copies, &sources, &chosen, &mut queue, &handle);
        let mut pump = Self {
            connection,
            queue,
            handle,
            stop,
            session,
            buffers,
            armed: None,
            free: (1..RING).collect(),
            returned: Arc::new(Mutex::new(Vec::with_capacity(RING))),
            slot: Arc::clone(slot),
            counters: Arc::clone(counters),
            cursor: Arc::clone(cursor),
            pointer,
            size,
            epoch: monotonic_nanos(),
            next_id: 0,
        };
        pump.arm(0);
        pump.arm_pointer();
        Ok((pump, format))
    }

    fn run(&mut self, state: &mut State) -> Result<(), Error> {
        loop {
            self.connection.flush()?;
            if let Some(guard) = self.queue.prepare_read() {
                let mut watched = [
                    PollFd::new(&self.connection, PollFlags::IN),
                    PollFd::new(&self.stop, PollFlags::IN),
                ];
                poll(&mut watched, None)?;
                if !watched[1].revents().is_empty() {
                    return Ok(());
                }
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error.into()),
                }
            }
            self.queue.dispatch_pending(state)?;
            self.service(state)?;
        }
    }

    fn service(&mut self, state: &mut State) -> Result<(), Error> {
        if state.screen.stopped {
            return Err(Error::SessionStopped);
        }
        if state.screen.size != Some((self.size.width(), self.size.height())) {
            return Err(Error::ConstraintsChanged);
        }
        let (ready, failed, presented) = state.screen.take_frame();
        if let Some(reason) = failed {
            tracing::warn!(%reason, "the compositor refused to fill a buffer");
            self.counters.failed.fetch_add(1, Ordering::Relaxed);
            if let Some((index, frame)) = self.armed.take() {
                frame.destroy();
                self.arm(index);
            }
        } else if ready {
            self.publish(presented.unwrap_or_else(monotonic_nanos))?;
        }
        self.service_pointer(state);
        Ok(())
    }

    /// Re-arms before publishing, so that a capture request is outstanding at
    /// every vertical blank rather than only after the encoder has been fed.
    fn publish(&mut self, presented: u64) -> Result<(), Error> {
        let Some((index, frame)) = self.armed.take() else {
            return Ok(());
        };
        frame.destroy();
        self.counters.captured.fetch_add(1, Ordering::Relaxed);
        let frame = self.wrap(index, presented)?;

        self.reclaim();
        if let Some(next) = self.free.pop() {
            self.arm(next);
            self.store(frame);
        } else {
            // Nothing is free only when the slot still holds a frame the
            // encoder has not taken. Replacing it hands that buffer straight
            // back, so the arm happens one step later rather than not at all.
            self.counters.starved.fetch_add(1, Ordering::Relaxed);
            self.store(frame);
            self.reclaim();
            let next = self
                .free
                .pop()
                .expect("a consumer is holding more than one frame, which Frames::next forbids");
            self.arm(next);
        }
        Ok(())
    }

    fn wrap(&mut self, index: usize, presented: u64) -> Result<Frame, Error> {
        let id = self.next_id;
        self.next_id += 1;
        Ok(Frame {
            id,
            captured: Timestamp::from_session_micros(presented.saturating_sub(self.epoch) / 1000),
            presented_nanos: presented,
            fd: self.buffers[index].object.fd_for_plane(0)?,
            lease: Lease {
                index,
                returned: Arc::clone(&self.returned),
            },
        })
    }

    fn store(&self, frame: Frame) {
        if self.slot.store(frame).is_some() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn reclaim(&mut self) {
        self.free.append(
            &mut self
                .returned
                .lock()
                .expect("the ring mutex is never held across a panic"),
        );
    }

    fn arm(&mut self, index: usize) {
        let frame = self.session.create_frame(&self.handle, Target::Screen);
        frame.attach_buffer(&self.buffers[index].buffer);
        #[expect(
            clippy::cast_possible_wrap,
            reason = "a display is far smaller than the positive range of an i32"
        )]
        frame.damage_buffer(0, 0, self.size.width() as i32, self.size.height() as i32);
        frame.capture();
        self.armed = Some((index, frame));
    }

    fn service_pointer(&mut self, state: &mut State) {
        let mut image = None;
        if let Some(pointer) = &mut self.pointer {
            let (ready, failed, _) = state.cursor.take_frame();
            if let Some(reason) = &failed {
                tracing::debug!(%reason, "the compositor refused to fill the cursor buffer");
            }
            if (ready || failed.is_some())
                && let Some(frame) = pointer.armed.take()
            {
                frame.destroy();
            }
            if ready {
                match pointer.read() {
                    Ok(picture) => image = Some(picture),
                    Err(error) => tracing::warn!(%error, "cannot read the captured cursor"),
                }
            }
        }
        if image.is_some() {
            state.pointer_changed = true;
        }
        if std::mem::take(&mut state.pointer_changed) {
            let mut cursor = self
                .cursor
                .lock()
                .expect("the cursor mutex is never held across a panic");
            let serial = cursor.serial + 1;
            *cursor = Cursor {
                image: image.or_else(|| cursor.image.clone()),
                serial,
                ..state.pointer.clone()
            };
        }
        self.arm_pointer();
    }

    fn arm_pointer(&mut self) {
        let Some(pointer) = &mut self.pointer else {
            return;
        };
        if pointer.armed.is_some() {
            return;
        }
        let frame = pointer.session.create_frame(&self.handle, Target::Cursor);
        frame.attach_buffer(&pointer.buffer);
        #[expect(
            clippy::cast_possible_wrap,
            reason = "a cursor is a few tens of pixels across"
        )]
        frame.damage_buffer(0, 0, pointer.size.0 as i32, pointer.size.1 as i32);
        frame.capture();
        pointer.armed = Some(frame);
    }
}

impl State {
    fn find_output(&self, wanted: &str) -> Result<(wl_output::WlOutput, Framerate), Error> {
        let output = self
            .outputs
            .iter()
            .find(|output| {
                self.names
                    .get(&output.id().protocol_id())
                    .is_some_and(|name| name == wanted)
            })
            .ok_or_else(|| Error::NoSuchOutput(wanted.to_owned()))?;
        let refresh = self
            .refresh
            .get(&output.id().protocol_id())
            .copied()
            .and_then(Framerate::from_millihertz)
            .ok_or_else(|| Error::NoSuchOutput(wanted.to_owned()))?;
        Ok((output.clone(), refresh))
    }
}

/// Picks a pixel format and memory layout both the compositor and the encoder
/// accept.
///
/// The scaler advertises exactly one layout per format, the compressed one this
/// GPU prefers, and refuses a plain linear buffer. Negotiating against the
/// compositor's list alone therefore produces an import failure at run time
/// rather than a clear one here.
fn choose_layout(offered: &[(u32, Vec<u64>)]) -> Result<(Format, Modifier), Error> {
    let importable = crate::encode::import_layouts()?;
    // Opaque before alpha: the desktop has no transparency to carry and the
    // encoder discards the channel either way.
    let preference = [
        Format::Xrgb8888,
        Format::Xbgr8888,
        Format::Argb8888,
        Format::Abgr8888,
    ];
    for format in preference {
        let Some((_, modifiers)) = offered.iter().find(|(code, _)| *code == format as u32) else {
            continue;
        };
        let shared = modifiers
            .iter()
            .copied()
            .filter(|modifier| *modifier != MODIFIER_INVALID)
            .find(|modifier| {
                importable
                    .get(&(format as u32))
                    .is_some_and(|m| m.contains(modifier))
            });
        if let Some(modifier) = shared {
            return Ok((format, Modifier::from(modifier)));
        }
    }
    Err(Error::NoSharedFormat)
}

impl Slab {
    fn allocate(
        gbm: &gbm::Device<std::fs::File>,
        size: Size,
        format: Format,
        modifier: Modifier,
        dmabuf: &ZwpLinuxDmabufV1,
        handle: &QueueHandle<State>,
    ) -> Result<Self, Error> {
        let object = gbm.create_buffer_object_with_modifiers2::<()>(
            size.width(),
            size.height(),
            format,
            [modifier].into_iter(),
            BufferObjectFlags::RENDERING,
        )?;

        let params = dmabuf.create_params(handle, ());
        let modifier = u64::from(object.modifier());
        #[expect(
            clippy::cast_possible_wrap,
            reason = "a buffer object never reports more planes than an i32 holds"
        )]
        for plane in 0..object.plane_count() as i32 {
            let fd = object.fd_for_plane(plane)?;
            #[expect(
                clippy::cast_sign_loss,
                reason = "the plane index is counted up from zero"
            )]
            params.add(
                fd.as_fd(),
                plane as u32,
                object.offset(plane),
                object.stride_for_plane(plane),
                (modifier >> 32) as u32,
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the protocol carries a modifier as two halves"
                )]
                (modifier as u32),
            );
        }
        #[expect(
            clippy::cast_possible_wrap,
            reason = "a display is far smaller than the positive range of an i32"
        )]
        let buffer = params.create_immed(
            size.width() as i32,
            size.height() as i32,
            format as u32,
            zwp_linux_buffer_params_v1::Flags::empty(),
            handle,
            (),
        );
        Ok(Self { object, buffer })
    }

    /// Where each plane sits, and how long the descriptor is.
    ///
    /// Every plane of one buffer object shares a single descriptor, so the
    /// encoder imports the object once and reads the plane offsets from the
    /// video metadata. That is checked here rather than assumed, because a
    /// split object would be imported as its first plane alone and produce a
    /// picture nobody could explain.
    fn layout(&self) -> Result<(Vec<Plane>, usize), Error> {
        let mut planes = Vec::new();
        let mut inode = None;
        #[expect(
            clippy::cast_possible_wrap,
            reason = "a buffer object never reports more planes than an i32 holds"
        )]
        for plane in 0..self.object.plane_count() as i32 {
            let fd = self.object.fd_for_plane(plane)?;
            let stat = rustix::fs::fstat(&fd)?;
            if *inode.get_or_insert(stat.st_ino) != stat.st_ino {
                return Err(Error::SplitBufferObject);
            }
            planes.push(Plane {
                offset: self.object.offset(plane),
                stride: self.object.stride_for_plane(plane),
            });
        }
        let fd = self.object.fd_for_plane(0)?;
        let length = rustix::fs::seek(&fd, rustix::fs::SeekFrom::End(0))?;
        Ok((planes, usize::try_from(length).unwrap_or(usize::MAX)))
    }
}

impl PointerCapture {
    /// Sets up the separate cursor session, or gives up quietly.
    ///
    /// A pointer that is not over this output, or a compositor that declines
    /// the session, costs the caller the cursor picture and nothing else, so
    /// this reports the reason rather than failing the whole capture.
    fn setup(
        state: &mut State,
        copies: &ExtImageCopyCaptureManagerV1,
        sources: &ExtOutputImageCaptureSourceManagerV1,
        output: &wl_output::WlOutput,
        queue: &mut EventQueue<State>,
        handle: &QueueHandle<State>,
    ) -> Option<Self> {
        let seat = state.seat.clone()?;
        let shm = state.shm.clone()?;
        let source = sources.create_source(output, handle, ());
        let pointer = seat.get_pointer(handle, ());
        let cursor = copies.create_pointer_cursor_session(&source, &pointer, handle, ());
        let session = cursor.get_capture_session(handle, Target::Cursor);
        for _ in 0..2 {
            if queue.roundtrip(state).is_err() {
                return None;
            }
        }
        if !state.cursor.done || state.cursor.stopped {
            tracing::info!("the compositor offered no cursor session");
            return None;
        }
        if !state.cursor.shm_formats.contains(&wl_shm::Format::Argb8888) {
            tracing::info!("the cursor session offers no ARGB layout");
            return None;
        }
        let (width, height) = state.cursor.size?;
        let stride = width.checked_mul(4)?;
        let length = stride.checked_mul(height)?;

        let storage = std::fs::File::from(
            rustix::fs::memfd_create("tachyx-cursor", rustix::fs::MemfdFlags::CLOEXEC).ok()?,
        );
        storage.set_len(u64::from(length)).ok()?;
        #[expect(
            clippy::cast_possible_wrap,
            reason = "a cursor is a few tens of pixels across"
        )]
        let pool = shm.create_pool(storage.as_fd(), length as i32, handle, ());
        #[expect(
            clippy::cast_possible_wrap,
            reason = "a cursor is a few tens of pixels across"
        )]
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
            handle,
            (),
        );
        pool.destroy();
        Some(Self {
            session,
            buffer,
            storage,
            size: (width, height),
            armed: None,
        })
    }

    fn read(&self) -> Result<CursorImage, std::io::Error> {
        let mut pixels = vec![0u8; (self.size.0 * self.size.1 * 4) as usize];
        self.storage.read_exact_at(&mut pixels, 0)?;
        Ok(CursorImage {
            width: self.size.0,
            height: self.size.1,
            pixels,
        })
    }
}
