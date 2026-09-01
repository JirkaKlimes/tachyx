use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSrc};
use gstreamer_gl::GL_DISPLAY_CONTEXT_TYPE;
use tachyx_glctx::AdoptedContext;

use super::Error;

/// The context name a GStreamer OpenGL element looks under for a context the
/// application already owns. GStreamer names no constant for it.
const APP_CONTEXT_TYPE: &str = "gst.gl.app_context";

/// Every caps filter here is load-bearing; see the crate documentation.
const DESCRIPTION: &str = concat!(
    "appsrc name=source is-live=true format=time do-timestamp=false ",
    "! h264parse ",
    "! vah264dec ",
    "! video/x-raw(memory:DMABuf) ",
    "! glupload ",
    "! glcolorconvert ",
    "! appsink name=sink sync=false max-buffers=1 drop=true ",
    "caps=video/x-raw(memory:GLMemory),format=RGBA,texture-target=2D",
);

/// The decode pipeline, from encoded bytes to a texture in Flutter's share
/// group.
#[derive(Debug)]
pub(super) struct Decoder {
    pipeline: gstreamer::Pipeline,
    source: AppSrc,
    sink: AppSink,
}

impl Decoder {
    /// Builds the pipeline. It stays in the null state until [`Decoder::play`],
    /// because the OpenGL elements cannot negotiate before the embedder's
    /// context has been adopted, and that only happens once Flutter asks for a
    /// texture.
    pub(super) fn build() -> Result<Self, Error> {
        gstreamer::init().map_err(Error::Gstreamer)?;
        let pipeline = gstreamer::parse::launch(DESCRIPTION)
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

    /// Hands the pipeline the embedder's display and render context, then starts
    /// it. The OpenGL elements find both without a bus round trip, because a bin
    /// passes a context it is given to every element it holds.
    pub(super) fn play(&self, adopted: &AdoptedContext) -> Result<(), Error> {
        let mut display = gstreamer::Context::new(GL_DISPLAY_CONTEXT_TYPE, true);
        display
            .get_mut()
            .expect("a freshly built context is unshared")
            .structure_mut()
            .set(GL_DISPLAY_CONTEXT_TYPE, adopted.display());
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
        access_unit: &[u8],
        arrived: gstreamer::ClockTime,
    ) -> Result<(), Error> {
        let mut buffer = gstreamer::Buffer::from_mut_slice(access_unit.to_vec());
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
