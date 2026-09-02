allprojects {
    repositories {
        google()
        mavenCentral()
    }
}

val newBuildDir: Directory =
    rootProject.layout.buildDirectory
        .dir("../../build")
        .get()
rootProject.layout.buildDirectory.value(newBuildDir)

subprojects {
    val newSubprojectBuildDir: Directory = newBuildDir.dir(project.name)
    project.layout.buildDirectory.value(newSubprojectBuildDir)
}

// Compile every module against the one SDK platform the dev shell provides.
//
// A plugin that names an older platform sends Gradle to sdkmanager, which cannot
// write into the read-only Nix store, so the build fails on a download it does
// not need. Compiling against a newer platform is always allowed, so pinning
// them all here costs nothing and removes the failure. The extension is reached
// without naming its type, because the Android Gradle Plugin's classes are not
// on the root build script's classpath.
//
// This must come before the block below: that one evaluates `:app` on the spot,
// and a project that has already been evaluated refuses a new callback.
subprojects {
    afterEvaluate {
        extensions.findByName("android")?.withGroovyBuilder {
            "compileSdkVersion"(36)
        }
    }
}

// Do not build irondash_engine_context's Rust half for Android.
//
// The published 0.5.5 builds it through cargokit, whose Gradle script calls
// `Project.exec`, removed in Gradle 9. The task therefore cannot run here at
// all. It also has nothing to do: that crate exists to hand a Rust GStreamer
// pipeline the engine's texture registry, which is the Linux present path.
// Android presents through a surface producer and never reaches it, so the
// library it would build is dead weight in the package.
//
// Remove this when the plugin moves to a release that builds through native
// assets, as the unpublished 0.6.0 already does.
subprojects {
    if (name == "irondash_engine_context") {
        tasks.matching { it.name.startsWith("cargokitCargoBuild") }.configureEach {
            enabled = false
        }
    }
}

subprojects {
    project.evaluationDependsOn(":app")
}

tasks.register<Delete>("clean") {
    delete(rootProject.layout.buildDirectory)
}
