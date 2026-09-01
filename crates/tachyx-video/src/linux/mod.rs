mod decoder;
mod stats;

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use gstreamer_gl::gl_video_frame::Readable;
use gstreamer_gl::prelude::*;
use gstreamer_gl::{GLSyncMeta, GLVideoFrame};
use gstreamer_video::VideoInfo;
use irondash_texture::{
    BoxedGLTexture, GLTexture, GLTextureProvider, PayloadProvider, SendableTexture, Texture,
};
use tachyx_glctx::AdoptedContext;

use decoder::Decoder;
use stats::Window;
pub use stats::{Latency, Stats};

/// `GL_TEXTURE_2D`. Flutter's Impeller renderer ignores the target a callback
/// returns and samples every external texture as this one, so returning any
/// other target draws nothing.
const TEXTURE_2D: u32 = 0x0DE1;

/// How many drawn frames stay alive.
///
/// The Linux embedder passes no destruction callback with a texture, so nothing
/// ever tells us a frame has been drawn. Releasing a frame as soon as the next
/// one is asked for would hand its buffer back to the decoder while the
/// rasteriser may still be reading it. Holding the last three means a buffer is
/// only recycled once two further callbacks have each returned a different
/// texture.
const HELD_FRAMES: usize = 3;

/// Why the present path could not be built or run.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// GStreamer itself would not start.
    #[error("GStreamer would not start: {0}")]
    Gstreamer(#[source] gstreamer::glib::Error),
    /// An element is missing, or two of them will not link.
    #[error("the decode pipeline would not build: {0}")]
    Build(#[source] gstreamer::glib::Error),
    /// The pipeline refused to run, which on this path almost always means the
    /// decoder cannot export a shared buffer.
    #[error("the decode pipeline would not start")]
    Start,
    /// The decoder would not accept an access unit.
    #[error("the decoder would not take an access unit: {0}")]
    Push(#[source] gstreamer::FlowError),
    /// Flutter refused to register the texture.
    #[error("Flutter would not register the texture: {0}")]
    Register(#[source] irondash_texture::Error),
}

/// A decoded video stream, drawn by Flutter as an external texture.
///
/// Build one on Flutter's platform thread, put its [`VideoTexture::id`] in a
/// `Texture` widget, and hand it access units. Nothing decodes until the widget
/// is on screen: the pipeline needs the embedder's OpenGL context, and the only
/// place that context is reachable is inside the callback Flutter makes when it
/// first draws the widget.
pub struct VideoTexture {
    shared: Arc<Shared>,
    texture: Arc<SendableTexture<BoxedGLTexture>>,
    id: i64,
}

impl VideoTexture {
    /// Registers a texture with the engine and prepares the decode pipeline.
    ///
    /// `engine_handle` comes from the `irondash_engine_context` Dart plugin.
    ///
    /// # Errors
    /// If GStreamer will not start, an element is missing, or the engine
    /// rejects the texture.
    ///
    /// # Panics
    /// If called off Flutter's platform thread, where there is no run loop.
    pub fn new(engine_handle: i64) -> Result<Self, Error> {
        let shared = Arc::new(Shared {
            pipeline: Decoder::build()?,
            start: Instant::now(),
            context: OnceLock::new(),
            started: Once::new(),
            failure: OnceLock::new(),
            notify: OnceLock::new(),
            state: Mutex::new(State {
                waiting: Some(VecDeque::new()),
                ..State::default()
            }),
        });

        let listener = Arc::downgrade(&shared);
        shared.pipeline.on_frame(move |sample| {
            if let Some(shared) = listener.upgrade() {
                shared.receive(sample);
            }
        });
        let reporter = Arc::downgrade(&shared);
        shared.pipeline.on_error(move |failure| {
            if let Some(shared) = reporter.upgrade() {
                shared.fail(failure);
            }
        });

        let texture =
            Texture::new_with_provider(engine_handle, shared.clone()).map_err(Error::Register)?;
        let id = texture.id();
        let texture = texture.into_sendable_texture();
        assert!(
            shared.notify.set(texture.clone()).is_ok(),
            "the notifier is set once, here"
        );

        Ok(Self {
            shared,
            texture,
            id,
        })
    }

    /// The identifier a Flutter `Texture` widget draws.
    #[must_use]
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Hands one encoded access unit to the decoder.
    ///
    /// Also asks Flutter to draw. Until the widget is on screen that ask is
    /// dropped, which is what keeps the pipeline stopped rather than decoding
    /// into a texture nobody reads.
    ///
    /// # Errors
    /// If the decoder rejects the access unit.
    pub fn push(&self, access_unit: Vec<u8>) -> Result<(), Error> {
        let arrived = gstreamer::ClockTime::from_nseconds(
            u64::try_from(self.shared.start.elapsed().as_nanos()).unwrap_or(u64::MAX),
        );
        {
            let mut state = self.shared.locked();
            state.pushed += 1;
            match state.waiting.as_mut() {
                Some(waiting) => waiting.push_back((access_unit, arrived)),
                None => self.shared.pipeline.push(access_unit, arrived)?,
            }
        }
        self.texture.mark_frame_available();
        Ok(())
    }

    /// Tells the decoder the stream has ended.
    pub fn finish(&self) {
        self.shared.pipeline.finish();
    }

    /// The first thing that went wrong, if anything has.
    ///
    /// A pipeline that cannot negotiate posts one message and then goes quiet.
    /// Without somewhere to read it, a client that shows no picture gives no
    /// reason for it either.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        self.shared.failure.get().cloned()
    }

    /// What the path has done so far.
    #[must_use]
    pub fn stats(&self) -> Stats {
        let state = self.shared.locked();
        Stats {
            pushed: state.pushed,
            decoded: state.decoded,
            presented: state.presented,
            superseded: state.superseded,
            decode: state.decode.summary(),
            present: state.present.summary(),
        }
    }
}

impl fmt::Debug for VideoTexture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VideoTexture")
            .field("id", &self.id)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// Everything both threads touch: the raster thread that draws and the
/// GStreamer thread that decodes.
struct Shared {
    pipeline: Decoder,
    start: Instant,
    context: OnceLock<AdoptedContext>,
    started: Once,
    failure: OnceLock<String>,
    notify: OnceLock<Arc<SendableTexture<BoxedGLTexture>>>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// Access units pushed before the pipeline could run, with the moment each
    /// arrived. Emptied and set to `None` when the pipeline starts, under the
    /// lock, which is also what routes a later push straight to the decoder. A
    /// push that overtook this queue would hand the decoder a frame before the
    /// key frame it refers back to.
    waiting: Option<VecDeque<(Vec<u8>, gstreamer::ClockTime)>>,
    /// The caps the pipeline currently produces, and their parsed form.
    format: Option<(gstreamer::Caps, VideoInfo)>,
    /// The newest frame Flutter has not asked for yet.
    pending: Option<gstreamer::Sample>,
    /// The frames Flutter may still be reading.
    held: VecDeque<Arc<Drawn>>,
    pushed: u64,
    decoded: u64,
    presented: u64,
    superseded: u64,
    decode: Window,
    present: Window,
}

impl fmt::Debug for Shared {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Shared")
            .field("context", &self.context)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn locked(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records the first thing that went wrong, and only the first: later
    /// messages are almost always that one failure repeating downstream.
    fn fail(&self, failure: String) {
        tracing::error!(%failure, "the decode and present path failed");
        let _ = self.failure.set(failure);
    }

    /// Adopts the embedder's context and starts decoding. Runs once, on the
    /// raster thread, inside the first texture callback.
    fn start(&self) {
        self.started.call_once(|| {
            let adopted = match AdoptedContext::adopt() {
                Ok(adopted) => adopted,
                Err(error) => {
                    self.fail(format!("could not adopt the embedder's OpenGL context: {error}"));
                    return;
                }
            };
            tracing::info!(api = ?adopted.api(), "adopted the embedder's OpenGL context");
            let adopted = self.context.get_or_init(|| adopted);
            if let Err(error) = self.pipeline.play(adopted) {
                self.fail(format!("the decode pipeline would not start: {error}"));
                return;
            }
            let mut state = self.locked();
            for (access_unit, arrived) in state.waiting.take().into_iter().flatten() {
                if let Err(error) = self.pipeline.push(access_unit, arrived) {
                    self.fail(format!("the decoder rejected a buffered access unit: {error}"));
                }
            }
        });
    }

    /// One decoded frame, on a GStreamer streaming thread.
    fn receive(&self, sample: gstreamer::Sample) {
        let elapsed = self.start.elapsed();
        {
            let mut state = self.locked();
            if let Some(latency) = latency_since_push(&sample, elapsed) {
                state.decode.record(latency);
            }
            state.decoded += 1;
            if state.pending.replace(sample).is_some() {
                state.superseded += 1;
            }
        }
        if let Some(texture) = self.notify.get() {
            texture.mark_frame_available();
        }
    }

    /// Maps the newest frame's texture and waits for the decoder's work on it to
    /// finish, on the raster thread with the embedder's context current.
    fn draw(
        &self,
        state: &mut State,
        sample: &gstreamer::Sample,
    ) -> Result<Arc<Drawn>, gstreamer::glib::BoolError> {
        let caps = sample
            .caps()
            .ok_or_else(|| gstreamer::glib::bool_error!("the decoded frame carries no caps"))?;
        if state.format.as_ref().is_none_or(|(known, _)| known != caps) {
            state.format = Some((caps.to_owned(), VideoInfo::from_caps(caps)?));
        }
        let (_, info) = state
            .format
            .as_ref()
            .expect("the format was just filled in");

        let buffer = sample
            .buffer_owned()
            .ok_or_else(|| gstreamer::glib::bool_error!("the decoded frame carries no buffer"))?;
        let frame = GLVideoFrame::from_buffer_readable(buffer, info).map_err(|_| {
            gstreamer::glib::bool_error!("the decoded frame is not an OpenGL frame")
        })?;

        // The decoder's colour conversion runs on GStreamer's own context. This
        // makes the rasteriser's context wait for it, rather than sampling a
        // half-written texture.
        if let (Some(adopted), Some(meta)) =
            (self.context.get(), frame.buffer().meta::<GLSyncMeta>())
        {
            meta.wait(adopted.context());
        }

        Ok(Arc::new(Drawn {
            name: frame.texture_id(0)?,
            width: i32::try_from(frame.width()).unwrap_or(i32::MAX),
            height: i32::try_from(frame.height()).unwrap_or(i32::MAX),
            _mapping: frame,
        }))
    }
}

impl PayloadProvider<BoxedGLTexture> for Shared {
    fn get_payload(&self) -> BoxedGLTexture {
        self.start();
        let elapsed = self.start.elapsed();
        let mut state = self.locked();
        if let Some(sample) = state.pending.take() {
            let latency = latency_since_push(&sample, elapsed);
            match self.draw(&mut state, &sample) {
                Ok(drawn) => {
                    if let Some(latency) = latency {
                        state.present.record(latency);
                    }
                    state.presented += 1;
                    state.held.push_back(drawn);
                    while state.held.len() > HELD_FRAMES {
                        state.held.pop_front();
                    }
                }
                Err(error) => self.fail(format!("could not draw a decoded frame: {error}")),
            }
        }
        match state.held.back() {
            Some(drawn) => Box::new(Held(drawn.clone())),
            None => Box::new(Nothing(0)),
        }
    }
}

/// A frame's texture, and the mapping that keeps it alive.
#[derive(Debug)]
struct Drawn {
    /// The mapping that holds the decoder's buffer. It is never read: dropping
    /// it is what returns the buffer to the decoder's pool, so its whole purpose
    /// is to stay alive for as long as Flutter might be drawing the texture.
    _mapping: GLVideoFrame<Readable>,
    name: u32,
    width: i32,
    height: i32,
}

/// A drawn frame handed to Flutter. Holding it holds the decoder's buffer.
#[derive(Debug)]
struct Held(Arc<Drawn>);

impl GLTextureProvider for Held {
    fn get(&self) -> GLTexture<'_> {
        GLTexture {
            target: TEXTURE_2D,
            name: &self.0.name,
            width: self.0.width,
            height: self.0.height,
        }
    }
}

/// The texture returned before the first frame exists.
///
/// The embedder gives a plugin no way to answer "nothing yet": the callback must
/// name a texture. Zero is OpenGL's null texture and draws nothing. It is
/// returned once, for the callback that adopts the context and so starts the
/// pipeline, and never again.
#[derive(Debug)]
struct Nothing(u32);

impl GLTextureProvider for Nothing {
    fn get(&self) -> GLTexture<'_> {
        GLTexture {
            target: TEXTURE_2D,
            name: &self.0,
            width: 1,
            height: 1,
        }
    }
}

/// How long ago the access unit behind this frame was pushed.
///
/// Every buffer is stamped with the moment it arrived, so the age of a frame is
/// read straight off it and no table maps frames back to pushes.
fn latency_since_push(sample: &gstreamer::Sample, elapsed: Duration) -> Option<Duration> {
    let pushed = sample.buffer()?.pts()?;
    Some(elapsed.saturating_sub(Duration::from_nanos(pushed.nseconds())))
}
