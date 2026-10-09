//! Gives the shared library its own name (SONAME) on Android. Without one,
//! whatever links against it (the Android library's JNI bridge, droidtop's)
//! records the path it was linked from on the build machine, and Android's
//! loader, which finds libraries by name inside the APK, cannot load it.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libwindowcast_client.so");
    }
}
