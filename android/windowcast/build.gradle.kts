plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.windowcast"
    compileSdk = 35

    defaultConfig {
        minSdk = 26
        consumerProguardFiles("consumer-rules.pro")
        externalNativeBuild {
            cmake {
                arguments += "-DWINDOWCAST_INCLUDE=${rootDir.parentFile.resolve("client-core/include")}"
                arguments += "-DWINDOWCAST_HOST_INCLUDE=${rootDir.parentFile.resolve("agent-android/include")}"
            }
        }
        ndk {
            // Both ABIs, as every windowcast and droidtop Android artifact.
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    externalNativeBuild {
        cmake {
            path = file("src/main/cpp/CMakeLists.txt")
        }
    }

    buildFeatures {
        // IAndroidHost: the Android host's binder, as a Shizuku user service.
        aidl = true
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
    // Shizuku's client (Apache-2.0), to start the Android host as the shell user. The embedding
    // app ships it (droidtop does); this library only compiles against it.
    compileOnly("dev.rikka.shizuku:api:13.1.5")
    testImplementation("junit:junit:4.13.2")
}
