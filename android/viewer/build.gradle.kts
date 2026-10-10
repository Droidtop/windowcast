plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// The release this viewer belongs to: the workspace version in the repository's
// Cargo.toml (release.sh sets it), and a code that grows with it
// (major * 10000 + minor * 100 + patch), so an install reports the real release
// and a newer one installs over an older one.
val workspaceVersion: String = rootDir.parentFile.resolve("Cargo.toml").readLines()
    .dropWhile { it.trim() != "[workspace.package]" }
    .firstOrNull { it.trim().startsWith("version") }
    ?.substringAfter('"')?.substringBefore('"')
    ?: error("no version in [workspace.package] of Cargo.toml")
val workspaceVersionCode: Int = workspaceVersion.split('.').map { it.toInt() }
    .let { (major, minor, patch) -> major * 10000 + minor * 100 + patch }

android {
    namespace = "dev.windowcast.viewer"
    compileSdk = 35

    defaultConfig {
        applicationId = "dev.windowcast.viewer"
        minSdk = 26
        targetSdk = 34
        versionCode = workspaceVersionCode
        versionName = workspaceVersion
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    // Debug builds share one key when android/viewer/debug.keystore exists (the Android
    // debug alias and passwords), so a new debug APK installs over an older one.
    signingConfigs {
        getByName("debug") {
            val shared = file("debug.keystore")
            if (shared.exists()) {
                storeFile = shared
                storePassword = "android"
                keyAlias = "androiddebugkey"
                keyPassword = "android"
            }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

kotlin {
    jvmToolchain(17)
}

dependencies {
    implementation(project(":windowcast"))
    // Custom Tabs, for signing in with an identity provider's page.
    implementation("androidx.browser:browser:1.8.0")
}
