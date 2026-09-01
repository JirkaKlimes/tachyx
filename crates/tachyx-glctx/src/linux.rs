use std::ffi::c_void;

use gstreamer_gl::{GLAPI, GLContext, GLPlatform, prelude::GLContextExt};
use gstreamer_gl_egl::GLDisplayEGL;

#[expect(
    unsafe_code,
    reason = "declaring the two EGL reads; both are marked safe because neither takes an argument, so no caller can pass either one anything to misuse"
)]
#[link(name = "EGL")]
unsafe extern "C" {
    safe fn eglGetCurrentDisplay() -> *mut c_void;
    safe fn eglGetCurrentContext() -> *mut c_void;
}

/// Why the embedder's context could not be adopted.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// EGL reports no current context on the calling thread.
    #[error("no EGL context is current on this thread")]
    NoCurrentContext,
    /// GStreamer would not take the display handle.
    #[error("GStreamer refused the EGL display: {0}")]
    Display(#[source] gstreamer_gl::glib::BoolError),
    /// GStreamer would not take the context handle.
    #[error("GStreamer refused the EGL context")]
    Context,
    /// The context could not be marked as belonging to the calling thread.
    #[error("could not claim the EGL context for this thread: {0}")]
    Activate(#[source] gstreamer_gl::glib::BoolError),
    /// GStreamer could not read the context's capabilities.
    #[error("could not read the EGL context's capabilities: {0}")]
    Capabilities(#[source] gstreamer_gl::glib::Error),
}

/// The embedder's graphics context, described so a GStreamer pipeline can share
/// textures with it.
#[derive(Debug)]
pub struct AdoptedContext {
    display: GLDisplayEGL,
    context: GLContext,
}

impl AdoptedContext {
    /// Reads the EGL display and context current on the calling thread.
    ///
    /// Call this from inside Flutter's texture callback and nowhere else. The
    /// embedder's render context is current on no other thread, and does not
    /// exist at all when a plugin is registered.
    ///
    /// The returned context is a description of a context someone else owns. It
    /// is never made current and never destroyed here; GStreamer creates its own
    /// context that shares object names with this one.
    ///
    /// # Errors
    /// If no context is current, or if GStreamer rejects either handle.
    pub fn adopt() -> Result<Self, Error> {
        let display_handle = eglGetCurrentDisplay();
        let context_handle = eglGetCurrentContext();
        if display_handle.is_null() || context_handle.is_null() {
            return Err(Error::NoCurrentContext);
        }

        #[expect(
            unsafe_code,
            reason = "one of the two handle adoptions this crate exists for; the lint stays on so a third is caught"
        )]
        // SAFETY: `eglGetCurrentDisplay` returned this handle a moment ago on
        // this thread, so it names a display that EGL has initialised. The
        // embedder holds it for the life of the engine and this call only takes
        // a reference to it, so it cannot be terminated while the wrapper lives.
        let display = unsafe { GLDisplayEGL::with_egl_display(display_handle as usize) }
            .map_err(Error::Display)?;

        let (api, _major, _minor) = GLContext::current_gl_api(GLPlatform::EGL);
        #[expect(
            unsafe_code,
            reason = "the second of the two handle adoptions this crate exists for"
        )]
        // SAFETY: `eglGetCurrentContext` returned this handle a moment ago on
        // this thread, so it names a live context on `display_handle`, which is
        // the display just wrapped. `api` is what EGL reports for that same
        // current context, so the wrapper does not claim capabilities the
        // context lacks. A wrapped context is never destroyed by GStreamer, so
        // the embedder keeps sole ownership.
        let context = unsafe {
            GLContext::new_wrapped(&display, context_handle as usize, GLPlatform::EGL, api)
        }
        .ok_or(Error::Context)?;

        // Records the calling thread as the one the context belongs to, which is
        // what lets a later synchronisation wait run inline here rather than
        // being posted to a thread that would never run it. On a wrapped context
        // this changes no EGL state.
        context.activate(true).map_err(Error::Activate)?;
        context.fill_info().map_err(Error::Capabilities)?;

        Ok(Self { display, context })
    }

    /// The EGL display, for a pipeline's `gst.gl.GLDisplay` context.
    #[must_use]
    pub fn display(&self) -> &GLDisplayEGL {
        &self.display
    }

    /// The render context, for a pipeline's `gst.gl.app_context`.
    #[must_use]
    pub fn context(&self) -> &GLContext {
        &self.context
    }

    /// Whether the context reports an API that can hold `GL_RGBA8` textures.
    ///
    /// Flutter's Linux embedder states `GL_RGBA8` for every texture it is
    /// handed and reads back nothing to check it, so a context that cannot
    /// produce that format fails at draw time rather than here.
    #[must_use]
    pub fn api(&self) -> GLAPI {
        self.context.gl_api()
    }
}
