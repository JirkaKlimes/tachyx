use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSrc};
use gstreamer_gl::GL_DISPLAY_CONTEXT_TYPE;
use gstreamer_gl::prelude::ContextGLExt;
use tachyx_proto::control::{Codec, Codecs};
use tachyx_unsafe::glctx::AdoptedContext;

use super::Error;

/// The context name a GStreamer OpenGL element looks under for a context the
/// application already owns. GStreamer names no constant for it.
const APP_CONTEXT_TYPE: &str = "gst.gl.app_context";

/// What one codec is decoded with.
///
/// The elements are named rather than left to `decodebin`, so a machine that
/// cannot honour the caps below fails to negotiate and says so, instead of
/// quietly assembling a chain that copies every frame through main memory.
#[derive(Debug, Clone, Copy)]
struct Chain {
    /// The caps the source declares, which say that every buffer handed over is
    /// one whole access unit. Without them the parser cannot tell where a unit
    /// ends until the next one arrives, and holds every frame for exactly one
    /// frame period.
    source: &'static str,
    /// The parser between the source and the decoder.
    parser: &'static str,
    /// Decoders to try, best first.
    ///
    /// VA only, and for the same reason the encoder is: the caps below demand a
    /// shared buffer out of the decoder, and a decoder that cannot export one
    /// has nothing to link to.
    decoders: &'static [&'static str],
}

/// Every codec this client knows how to decode, and what it takes to do it.
const CHAINS: [(Codec, Chain); 2] = [
    (
        Codec::H264,
        Chain {
            source: "video/x-h264,stream-format=byte-stream,alignment=au",
            parser: "h264parse",
            decoders: &["vah264dec"],
        },
    ),
    (
        Codec::Av1,
        Chain {
            source: "video/x-av1,stream-format=obu-stream,alignment=tu",
            parser: "av1parse",
            decoders: &["vaav1dec"],
        },
    ),
];

impl Chain {
    /// The decoder element this machine really has, best first.
    fn decoder(self) -> Option<&'static str> {
        self.decoders
            .iter()
            .copied()
            .find(|name| gstreamer::ElementFactory::find(name).is_some())
    }

    /// Whether every element this chain names is installed.
    fn present(self) -> bool {
        gstreamer::ElementFactory::find(self.parser).is_some() && self.decoder().is_some()
    }
}

fn chain(codec: Codec) -> Chain {
    CHAINS
        .into_iter()
        .find_map(|(known, chain)| (known == codec).then_some(chain))
        .expect("every codec the protocol names has a chain")
}

/// The codecs this machine can decode.
///
/// # Errors
/// If GStreamer will not start, in which case nothing can be decoded at all.
pub fn decoders() -> Result<Codecs, Error> {
    gstreamer::init().map_err(Error::Gstreamer)?;
    Ok(CHAINS
        .into_iter()
        .filter(|(_, chain)| chain.present())
        .fold(Codecs::NONE, |found, (codec, _)| found.with(codec)))
}

/// The pipeline for one codec. Every caps filter in it is load-bearing; see the
/// crate documentation.
fn description(codec: Codec) -> Result<String, Error> {
    let chain = chain(codec);
    let decoder = chain.decoder().ok_or(Error::NoDecoder(codec))?;
    Ok(format!(
        "appsrc name=source is-live=true format=time do-timestamp=false \
         caps={source} \
         ! {parser} \
         ! {decoder} \
         ! video/x-raw(memory:DMABuf) \
         ! glupload \
         ! glcolorconvert \
         ! appsink name=sink sync=false max-buffers=1 drop=true \
         caps=video/x-raw(memory:GLMemory),format=RGBA,texture-target=2D",
        source = chain.source,
        parser = chain.parser,
    ))
}

/// The decode pipeline, from encoded bytes to a texture in Flutter's share
/// group.
#[derive(Debug)]
pub(super) struct Decoder {
    pipeline: gstreamer::Pipeline,
    source: AppSrc,
    sink: AppSink,
}

impl Decoder {
    /// Builds the pipeline for the codec the host chose. It stays in the null
    /// state until [`Decoder::play`], because the OpenGL elements cannot
    /// negotiate before the embedder's context has been adopted, and that only
    /// happens once Flutter asks for a texture.
    pub(super) fn build(codec: Codec) -> Result<Self, Error> {
        gstreamer::init().map_err(Error::Gstreamer)?;
        let pipeline = gstreamer::parse::launch(&description(codec)?)
            .map_err(Error::Build)?
            .downcast::<gstreamer::Pipeline>()
            .expect("a parsed pipeline description is a pipeline");
        let source = pipeline
            .by_name("source")
            .expect("the description names an appsrc")
            .downcast::<AppSrc>()
            .expect("the element named source is an appsrc");
        let sink = pipeline
            .by_name("sink")
            .expect("the description names an appsink")
            .downcast::<AppSink>()
            .expect("the element named sink is an appsink");
        Ok(Self {
            pipeline,
            source,
            sink,
        })
    }

    /// Calls `handler` on a GStreamer streaming thread for every decoded frame.
    pub(super) fn on_frame<F>(&self, handler: F)
    where
        F: Fn(gstreamer::Sample) + Send + 'static,
    {
        self.sink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gstreamer::FlowError::Eos)?;
                    handler(sample);
                    Ok(gstreamer::FlowSuccess::Ok)
                })
                .build(),
        );
    }

    /// Calls `handler` with the first error the pipeline posts.
    ///
    /// An element that cannot negotiate says so on the bus and nowhere else, so
    /// without this a pipeline that never starts is silent. That is the one
    /// failure worth surfacing: everything on this path is decided when the
    /// first frame is negotiated, and a pipeline that survives that runs.
    pub(super) fn on_error<F>(&self, handler: F)
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        self.pipeline
            .bus()
            .expect("a pipeline has a bus")
            .set_sync_handler(move |_, message| {
                if let gstreamer::MessageView::Error(failure) = message.view() {
                    handler(format!(
                        "{}: {}",
                        message
                            .src()
                            .map_or_else(|| "pipeline".to_owned(), |src| src.path_string().into()),
                        failure.error()
                    ));
                }
                gstreamer::BusSyncReply::Drop
            });
    }

    /// Hands the pipeline the embedder's display and render context, then starts
    /// it. The OpenGL elements find both without a bus round trip, because a bin
    /// passes a context it is given to every element it holds.
    pub(super) fn play(&self, adopted: &AdoptedContext) -> Result<(), Error> {
        let mut display = gstreamer::Context::new(GL_DISPLAY_CONTEXT_TYPE, true);
        display
            .get_mut()
            .expect("a freshly built context is unshared")
            .set_gl_display(Some(adopted.display()));
        self.pipeline.set_context(&display);

        let mut application = gstreamer::Context::new(APP_CONTEXT_TYPE, true);
        application
            .get_mut()
            .expect("a freshly built context is unshared")
            .structure_mut()
            .set("context", adopted.context());
        self.pipeline.set_context(&application);

        self.pipeline
            .set_state(gstreamer::State::Playing)
            .map_err(|_| Error::Start)?;
        Ok(())
    }

    /// Hands one access unit to the decoder, timestamped with the moment it
    /// arrived. Latency is read back off that timestamp when the frame is drawn,
    /// which is why nothing here tracks frames by identity.
    pub(super) fn push(
        &self,
        access_unit: Vec<u8>,
        arrived: gstreamer::ClockTime,
    ) -> Result<(), Error> {
        let mut buffer = gstreamer::Buffer::from_mut_slice(access_unit);
        buffer
            .get_mut()
            .expect("a freshly built buffer is unshared")
            .set_pts(arrived);
        self.source.push_buffer(buffer).map_err(Error::Push)?;
        Ok(())
    }

    /// Tells the decoder no more access units are coming.
    pub(super) fn finish(&self) {
        let _ = self.source.end_of_stream();
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}
