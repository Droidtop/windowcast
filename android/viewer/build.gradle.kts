plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.windowcast.viewer"
    compileSdk = 35

    defaultConfig {
        applicationId = "dev.windowcast.viewer"
        minSdk = 26
        targetSdk = 34
        versionCode = 2
        versionName = "0.6.0"
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
