package com.tachyx.tachyx

import android.app.PictureInPictureParams
import android.content.Context
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.graphics.Rect
import android.net.wifi.WifiManager
import android.os.Build
import android.util.Rational
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel
import io.flutter.plugins.pathprovider.PathProviderPlugin
import io.flutter.plugins.sharedpreferences.SharedPreferencesPlugin

class MainActivity : FlutterActivity() {
    /**
     * Held while this activity's window is visible.
     *
     * Without it the Wi-Fi power management on this hardware polices the
     * stream's UDP flow. Measured on a Galaxy S25 with counters on both ends:
     * about two seconds after the packet rate rises past roughly 150 packets a
     * second, the phone's Wi-Fi stack stops delivering that flow to the IP
     * layer, permanently, while small packets and new flows still pass. The
     * host then sees a connection that acknowledges nothing, its congestion
     * window fills, and the stream freezes. A plain UDP blast from the host
     * reproduces the cut with no QUIC involved, so no transport setting can
     * avoid it: the radio's own policy has to be asked to stand down, and this
     * lock is the sanctioned way to ask.
     *
     * `WIFI_MODE_FULL_LOW_LATENCY` also disables Wi-Fi power save. The system
     * binds it fully only while the holder is foreground with the screen on,
     * and a picture-in-picture thumbnail is not that: there the activity is
     * paused, and it is still visible and still decoding. So the lock is taken
     * for as long as the window is visible rather than for as long as it has
     * focus. A lock the system has downgraded still beats the alternative,
     * which is dropping it in the middle of a running stream.
     */
    private var wifiLock: WifiManager.WifiLock? = null

    /** Answers `tachyx/pip`, and reports the folding back over the same one. */
    private var pip: MethodChannel? = null

    /**
     * The shape a thumbnail would take, or null while the app must not fold.
     *
     * One field rather than a flag beside a shape, because armed without a
     * shape is not a state this can be in.
     */
    private var pipAspect: Rational? = null

    /**
     * Where the picture sits in the window, for the fold to shrink from.
     *
     * A hint and not a state: null just means the system crossfades the whole
     * window instead of morphing the picture into the thumbnail.
     */
    private var pipSource: Rect? = null

    override fun onStart() {
        super.onStart()
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        wifiLock = wifi
            .createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "tachyx:stream")
            .apply {
                setReferenceCounted(false)
                acquire()
            }
    }

    override fun onStop() {
        wifiLock?.release()
        wifiLock = null
        super.onStop()
    }

    /**
     * Registers what this client uses on Android, by name.
     *
     * Deliberately not `super`, whose whole body is a reflective call to the
     * generated registrant. That registrant walks every plugin in the pubspec in
     * one order and guards each with `catch (Exception)`. One of them is
     * `irondash_engine_context`, which is the Linux texture path and has no part
     * in the Android one: its class loads a native library that its own Gradle
     * build cannot produce here, because that build calls `Project.exec`, which
     * Gradle 9 removed. That is an `UnsatisfiedLinkError`, which is an `Error`
     * and not an `Exception`, so it escapes the guard and takes every plugin
     * after it down with it. Before this, nothing on Android could read a
     * directory or a preference, and a session could not be opened at all.
     *
     * A plugin added to the pubspec has to be added here as well. It says so
     * loudly if it is not: the first call into it fails with the name of the
     * channel nobody is answering.
     */
    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        flutterEngine.plugins.add(PathProviderPlugin())
        flutterEngine.plugins.add(SharedPreferencesPlugin())
        VideoSurface.attachTo(flutterEngine)
        pip = MethodChannel(flutterEngine.dartExecutor.binaryMessenger, PIP_CHANNEL).apply {
            setMethodCallHandler { call, result -> onPipCall(call, result) }
        }
    }

    private fun onPipCall(call: MethodCall, result: MethodChannel.Result) {
        when (call.method) {
            "arm" -> {
                arm(dimension(call, "width"), dimension(call, "height"), sourceRect(call))
                result.success(null)
            }
            "disarm" -> {
                disarm()
                result.success(null)
            }
            else -> result.notImplemented()
        }
    }

    private fun dimension(call: MethodCall, name: String) =
        call.argument<Number>(name)?.toInt() ?: 0

    /**
     * Where the picture sits in the window, or null when the client did not
     * say. A rectangle with nothing in it is a report that cannot be meant, so
     * it reads as no report rather than as a hint the system would reject.
     */
    private fun sourceRect(call: MethodCall): Rect? {
        val left = call.argument<Number>("left")?.toInt() ?: return null
        val top = call.argument<Number>("top")?.toInt() ?: return null
        val right = call.argument<Number>("right")?.toInt() ?: return null
        val bottom = call.argument<Number>("bottom")?.toInt() ?: return null
        if (right <= left || bottom <= top) return null
        return Rect(left, top, right, bottom)
    }

    /**
     * Lets the system fold this window into a thumbnail of the given shape.
     *
     * From API 31 the system does the folding on its own, at the moment the
     * user leaves, which is the only moment that looks right: the window
     * shrinks into the corner as part of the same animation that takes the user
     * away. Before that there is no such setting, so the leaving is answered by
     * hand in [onUserLeaveHint] and the shape is kept here for it.
     *
     * A device without the feature answers this and does nothing else. The
     * thumbnail is a comfort and not a session, so failing the call would only
     * make the client carry a second path to the same place.
     */
    private fun arm(width: Int, height: Int, source: Rect?) {
        if (!supportsPip) return
        val aspect = aspectRatio(width, height)
        pipAspect = aspect
        pipSource = source
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            setPictureInPictureParams(
                PictureInPictureParams.Builder()
                    .setAspectRatio(aspect)
                    .setSourceRectHint(source)
                    .setAutoEnterEnabled(true)
                    .build(),
            )
        }
    }

    /** Takes that permission back. */
    private fun disarm() {
        if (!supportsPip) return
        pipAspect = null
        pipSource = null
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            setPictureInPictureParams(
                PictureInPictureParams.Builder().setAutoEnterEnabled(false).build(),
            )
        }
    }

    /**
     * The user leaving for the home screen or another app, below API 31.
     *
     * This arrives while the activity is still resumed, which is the only state
     * the system enters the mode from. From API 31 the same transition is the
     * system's own business and entering it here as well would fold the window
     * twice.
     */
    override fun onUserLeaveHint() {
        super.onUserLeaveHint()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) return
        val aspect = pipAspect ?: return
        enterPictureInPictureMode(
            PictureInPictureParams.Builder()
                .setAspectRatio(aspect)
                .setSourceRectHint(pipSource)
                .build(),
        )
    }

    /**
     * Tells the client that the window has folded or unfolded.
     *
     * The client cannot know this by itself. The system folds the window
     * without asking once it has been allowed to, and the user unfolds it by
     * tapping the thumbnail, so both edges are reported from here.
     */
    override fun onPictureInPictureModeChanged(
        isInPictureInPictureMode: Boolean,
        newConfig: Configuration,
    ) {
        super.onPictureInPictureModeChanged(isInPictureInPictureMode, newConfig)
        pip?.invokeMethod("changed", isInPictureInPictureMode)
    }

    /**
     * Whether this device has the mode at all.
     *
     * A television, a device built for a small memory budget, and a car all
     * ship Android without it, and asking such a device to enter the mode
     * throws rather than declining.
     */
    private val supportsPip: Boolean
        get() = packageManager.hasSystemFeature(PackageManager.FEATURE_PICTURE_IN_PICTURE)

    /**
     * The picture's shape, held inside the band the platform accepts.
     *
     * Android takes nothing flatter than 2.39 to 1 nor narrower than its
     * reciprocal, and throws on the rest rather than trimming it, so a picture
     * outside the band is trimmed to the edge of it here. The result is a
     * thumbnail that crops a very wide host desktop, which is what the user
     * asked for by having one. A shape of zero is no shape and cannot be
     * divided, so a client that has not yet been told the picture's size gets
     * the common one until it has.
     */
    private fun aspectRatio(width: Int, height: Int): Rational {
        if (width <= 0 || height <= 0) return Rational(16, 9)
        val ratio = width.toDouble() / height.toDouble()
        return when {
            ratio > FLATTEST -> Rational(239, 100)
            ratio < 1 / FLATTEST -> Rational(100, 239)
            else -> Rational(width, height)
        }
    }

    companion object {
        private const val PIP_CHANNEL = "tachyx/pip"

        /** The widest a thumbnail may be against its height. */
        private const val FLATTEST = 2.39
    }
}
