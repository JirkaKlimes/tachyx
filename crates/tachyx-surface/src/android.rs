use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use jni::objects::{JClass, JObject};
use jni::sys::jlong;
use jni::JNIEnv;
use ndk::native_window::NativeWindow;

/// Every surface producer the Kotlin half has opened, by the identifier Flutter
/// gave it.
///
/// A table rather than a handle passed back to Kotlin, because the identifier is
/// already the name Dart uses for the same thing when it asks for a `Texture`
/// widget, and inventing a second one would mean keeping the two in step.
static PRODUCERS: LazyLock<Mutex<HashMap<i64, Arc<Producer>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// One Flutter surface producer, as seen from the decoder.
///
/// Long lived: it outlives every surface the producer hands out, so a decoder
/// holds one of these and asks it what to draw into rather than holding a
/// surface that may already have been replaced.
#[derive(Debug, Default)]
pub struct Producer {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    window: Option<Arc<NativeWindow>>,
    generation: u64,
}

impl Producer {
    /// The surface to draw into, if the producer has one.
    ///
    /// `None` while the application is in the background, where Flutter takes
    /// the surface back and the decoder has nowhere to put a frame.
    #[must_use]
    pub fn surface(&self) -> Option<Surface> {
        let state = self.locked();
        Some(Surface {
            window: Arc::clone(state.window.as_ref()?),
            generation: state.generation,
        })
    }

    fn locked(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn replace(&self, window: Option<NativeWindow>) {
        let mut state = self.locked();
        state.window = window.map(Arc::new);
        state.generation += 1;
    }
}

/// A surface, and how many the producer has had before it.
///
/// The count is the whole point of the type. A resize builds a new image reader
/// behind a new surface, so a decoder holding the old one draws into a buffer
/// nothing reads; comparing counts is how it notices.
#[derive(Debug, Clone)]
pub struct Surface {
    window: Arc<NativeWindow>,
    generation: u64,
}

impl Surface {
    /// The window the decoder writes into.
    #[must_use]
    pub fn window(&self) -> &NativeWindow {
        &self.window
    }

    /// How many surfaces this producer had before this one.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// The producer Flutter registered under `id`, if the Kotlin half has opened it.
#[must_use]
pub fn producer(id: i64) -> Option<Arc<Producer>> {
    let producers = PRODUCERS.lock().unwrap_or_else(PoisonError::into_inner);
    producers.get(&id).map(Arc::clone)
}

/// Records the surface a producer is currently offering.
///
/// Called from Kotlin when a producer is opened, when it is resized, and when
/// the application comes back to the foreground.
#[expect(
    unsafe_code,
    reason = "the call this crate exists to make, and the entry point Kotlin resolves it through; the lint stays on so a third one is caught"
)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tachyx_tachyx_VideoSurfaceNative_attach(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    id: jlong,
    surface: JObject<'_>,
) {
    // SAFETY: `surface` is whatever object Kotlin passed, and no type in either
    // language relates a Java object reference to a live `android.view.Surface`.
    // The Kotlin half passes its own producer's surface and calls this from
    // nowhere else, which is as close to a proof as this boundary admits. A
    // reference to anything else yields a null window, which is `None` here and
    // reads as a producer with nothing to draw into.
    let window = unsafe { NativeWindow::from_surface(env.get_raw(), surface.as_raw()) };
    let mut producers = PRODUCERS.lock().unwrap_or_else(PoisonError::into_inner);
    producers.entry(id).or_default().replace(window);
}

/// Records that a producer has taken its surface back, with the producer itself
/// still alive.
///
/// Flutter does this when the application goes to the background.
#[expect(
    unsafe_code,
    reason = "an entry point the Java virtual machine resolves by name, which is what the attribute is for"
)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tachyx_tachyx_VideoSurfaceNative_detach(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    id: jlong,
) {
    let producers = PRODUCERS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(producer) = producers.get(&id) {
        producer.replace(None);
    }
}

/// Forgets a producer entirely.
///
/// Flutter reuses texture identifiers, so an entry left behind would hand the
/// next session's surface to a decoder from this one.
#[expect(
    unsafe_code,
    reason = "an entry point the Java virtual machine resolves by name, which is what the attribute is for"
)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tachyx_tachyx_VideoSurfaceNative_close(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    id: jlong,
) {
    let mut producers = PRODUCERS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(producer) = producers.remove(&id) {
        producer.replace(None);
    }
}
