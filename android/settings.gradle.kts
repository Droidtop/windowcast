// The Android end of windowcast: `windowcast` is the library an Android
// client embeds (client-core through JNI, plus MediaCodec decoding);
// `viewer` is a small app for trying it against a host.
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "windowcast-android"
include(":windowcast", ":viewer")
