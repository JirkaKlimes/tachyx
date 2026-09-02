package com.tachyx.tachyx

import android.content.Context
import android.net.wifi.WifiManager
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugins.pathprovider.PathProviderPlugin
import io.flutter.plugins.sharedpreferences.SharedPreferencesPlugin

class MainActivity : FlutterActivity() {
    /**
     * Held while this activity is in the foreground.
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
     * `WIFI_MODE_FULL_LOW_LATENCY` also disables Wi-Fi power save, and it only
     * binds while the holder is foreground with the screen on, which is
     * exactly the life of a streaming session.
     */
    private var wifiLock: WifiManager.WifiLock? = null

    override fun onResume() {
        super.onResume()
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        wifiLock = wifi
            .createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "tachyx:stream")
            .apply {
                setReferenceCounted(false)
                acquire()
            }
    }

    override fun onPause() {
        wifiLock?.release()
        wifiLock = null
        super.onPause()
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
    }
}
