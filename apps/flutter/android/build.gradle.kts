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

subprojects {
    project.evaluationDependsOn(":app")
}

tasks.register<Delete>("clean") {
    delete(rootProject.layout.buildDirectory)
}
