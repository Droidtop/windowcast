# The JNI shim calls these by name.
-keep class dev.windowcast.Native { *; }

# The Android host's Shizuku user service: Shizuku makes it by its class name, with a Context.
-keep class dev.windowcast.host.AndroidHostService { <init>(android.content.Context); *; }
# The host's JNI bridge looks these up by name.
-keep class dev.windowcast.host.HostNative { *; }
