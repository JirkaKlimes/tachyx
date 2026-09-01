package com.tachyx.tachyx

import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugins.pathprovider.PathProviderPlugin
import io.flutter.plugins.sharedpreferences.SharedPreferencesPlugin

class MainActivity : FlutterActivity() {
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
