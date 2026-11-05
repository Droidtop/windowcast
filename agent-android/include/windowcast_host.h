/* windowcast's Android host (Droidtop/tracker#615): the C interface of
 * libwindowcast_host_android.so, which the Android library's
 * dev.windowcast.host.AndroidHost calls through its JNI bridge. The Android
 * side owns the virtual displays and the MediaCodec encoders; it hands frames
 * in and takes the host's commands out. Nothing here calls into Java. */
#ifndef WINDOWCAST_HOST_H
#define WINDOWCAST_HOST_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct AndroidHost WindowcastAndroidHost;

/* Serves on listen ("ADDR:PORT") with the identity in data_dir; PIN pairing
 * only with pairing. Null on failure, with the reason in error. */
WindowcastAndroidHost *windowcast_android_host_start(const char *data_dir, const char *listen, bool pairing,
                                                     char *error, size_t error_cap);
void windowcast_android_host_free(WindowcastAndroidHost *host);
/* The app windows now, as JSON [{"id","title","package","width","height"}]. 0, or -1. */
int64_t windowcast_android_host_set_windows(const WindowcastAndroidHost *host, const char *json);
/* The codecs the device encodes (1 H.264, 2 H.265, 3 AV1), most preferred first. */
void windowcast_android_host_set_encoders(const WindowcastAndroidHost *host, const uint32_t *codecs, size_t count);
/* One encoded frame; width and height when the size is new, else 0. 1 taken, 0 nobody streams it. */
int64_t windowcast_android_host_frame(const WindowcastAndroidHost *host, uint64_t window, const uint8_t *data,
                                      size_t len, uint64_t duration_us, uint32_t width, uint32_t height);
/* The window's app is gone. */
void windowcast_android_host_ended(const WindowcastAndroidHost *host, uint64_t window);
/* The next command as JSON ({"command":"open"|"close"|"keyframe"|"bitrate",...}), waiting up to
 * timeout_ms: its length, or 0 when none came. */
int64_t windowcast_android_host_next_command(const WindowcastAndroidHost *host, uint32_t timeout_ms, char *out,
                                             size_t cap);

#endif
