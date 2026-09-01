package com.tachyx.tachyx

import android.view.Surface
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel
import io.flutter.view.TextureRegistry

/**
 * The Android half of the decode and present path.
 *
 * It opens a surface producer, which on API 29 and above is an image reader
 * whose buffers reach the compositor's Vulkan backend as they are. The decoder
 * writes into that surface from Rust, so no frame ever passes through Dart or
 * through this file. All this does is open the producer, keep it the size the
 * decoder writes, and tell Rust which surface is current.
 *
 * The older surface texture is deliberately not used. Flutter's own source
 * describes the OpenGL to Vulkan interop it needs as costing one copy on the
 * device per frame and as existing only to migrate away from.
 */
class VideoSurface(private val textures: TextureRegistry) : MethodChannel.MethodCallHandler {
    private val producers = HashMap<Long, TextureRegistry.SurfaceProducer>()

    companion object {
        private const val CHANNEL = "tachyx/video"

        /** Answers `tachyx/video` for the life of [engine]. */
        fun attachTo(engine: FlutterEngine) {
            MethodChannel(engine.dartExecutor.binaryMessenger, CHANNEL)
                .setMethodCallHandler(VideoSurface(engine.renderer))
        }
    }

    override fun onMethodCall(call: MethodCall, result: MethodChannel.Result) {
        val id = call.argument<Number>("id")?.toLong()
        when (call.method) {
            "open" -> result.success(open(size(call, "width"), size(call, "height")))
            "resize" -> {
                if (id != null) resize(id, size(call, "width"), size(call, "height"))
                result.success(null)
            }
            "close" -> {
                if (id != null) close(id)
                result.success(null)
            }
            else -> result.notImplemented()
        }
    }

    private fun size(call: MethodCall, name: String) = call.argument<Number>(name)?.toInt() ?: 1

    /** Opens a producer sized for a [width] by [height] decoded picture. */
    private fun open(width: Int, height: Int): Long {
        val producer = textures.createSurfaceProducer()
        val id = producer.id()
        producer.setSize(width, height)
        producers[id] = producer
        // Flutter takes the surface back while the application is in the
        // background and offers a new one on the way in. The decoder is told
        // both, so that it stops drawing into a surface that is gone and picks
        // up the new one without losing what it has decoded.
        producer.setCallback(
            object : TextureRegistry.SurfaceProducer.Callback {
                override fun onSurfaceAvailable() {
                    VideoSurfaceNative.attach(id, producer.surface)
                }

                override fun onSurfaceCleanup() {
                    VideoSurfaceNative.detach(id)
                }
            },
        )
        VideoSurfaceNative.attach(id, producer.surface)
        return id
    }

    /**
     * Resizes the producer to what the decoder is writing.
     *
     * A different size builds a new image reader behind a new surface, so the
     * surface has to be read back and handed over again. Reading it is what
     * builds it.
     */
    private fun resize(id: Long, width: Int, height: Int) {
        val producer = producers[id] ?: return
        if (producer.width == width && producer.height == height) return
        producer.setSize(width, height)
        VideoSurfaceNative.attach(id, producer.surface)
    }

    /** Closes a producer and its texture. */
    private fun close(id: Long) {
        val producer = producers.remove(id) ?: return
        // Rust first: it drops its own reference to the window before the
        // producer takes the surface out from under it.
        VideoSurfaceNative.close(id)
        producer.setCallback(null)
        producer.release()
    }
}

/**
 * The Rust side of the same thing.
 *
 * `System.loadLibrary` is required and cannot be left to Dart. Dart reaches the
 * same library through `dlopen`, which loads it but registers nothing with the
 * virtual machine, so the three methods below would not resolve.
 */
private object VideoSurfaceNative {
    init {
        System.loadLibrary("tachyx_client_ffi")
    }

    /** Records the surface a producer is offering. */
    external fun attach(id: Long, surface: Surface)

    /** Records that the producer's surface has gone, with the producer alive. */
    external fun detach(id: Long)

    /** Forgets the producer entirely. */
    external fun close(id: Long)
}
