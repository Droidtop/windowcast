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
        versionCode = 1
        versionName = "0.4.0"
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
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
}
