/* The JNI bridge of dev.windowcast.host.HostNative over windowcast_host.h
 * (Droidtop/tracker#615), as windowcast_jni.c is the client's. */
#include <jni.h>
#include <string.h>
#include "windowcast_host.h"

#define HOST(h) ((const WindowcastAndroidHost *)(intptr_t)(h))
#define ERROR_CAP 512
#define COMMAND_CAP 16384

static __thread char last_error[ERROR_CAP];

JNIEXPORT jlong JNICALL
Java_dev_windowcast_host_HostNative_start(JNIEnv *env, jclass cls, jstring dataDir, jstring listen, jboolean pairing) {
    const char *dir = (*env)->GetStringUTFChars(env, dataDir, NULL);
    const char *at = (*env)->GetStringUTFChars(env, listen, NULL);
    last_error[0] = 0;
    WindowcastAndroidHost *host = windowcast_android_host_start(dir, at, pairing, last_error, sizeof last_error);
    (*env)->ReleaseStringUTFChars(env, listen, at);
    (*env)->ReleaseStringUTFChars(env, dataDir, dir);
    return (jlong)(intptr_t)host;
}

JNIEXPORT jstring JNICALL
Java_dev_windowcast_host_HostNative_lastError(JNIEnv *env, jclass cls) {
    return (*env)->NewStringUTF(env, last_error);
}

JNIEXPORT void JNICALL
Java_dev_windowcast_host_HostNative_free(JNIEnv *env, jclass cls, jlong host) {
    windowcast_android_host_free((WindowcastAndroidHost *)(intptr_t)host);
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_host_HostNative_setWindows(JNIEnv *env, jclass cls, jlong host, jstring json) {
    const char *text = (*env)->GetStringUTFChars(env, json, NULL);
    int64_t result = windowcast_android_host_set_windows(HOST(host), text);
    (*env)->ReleaseStringUTFChars(env, json, text);
    return result;
}

JNIEXPORT void JNICALL
Java_dev_windowcast_host_HostNative_setEncoders(JNIEnv *env, jclass cls, jlong host, jintArray codecs) {
    jsize count = (*env)->GetArrayLength(env, codecs);
    jint *ids = (*env)->GetIntArrayElements(env, codecs, NULL);
    uint32_t list[8];
    size_t n = 0;
    for (jsize i = 0; i < count && n < 8; i++) list[n++] = (uint32_t)ids[i];
    (*env)->ReleaseIntArrayElements(env, codecs, ids, JNI_ABORT);
    windowcast_android_host_set_encoders(HOST(host), list, n);
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_host_HostNative_frame(JNIEnv *env, jclass cls, jlong host, jlong window, jbyteArray data,
                                          jint length, jlong durationUs, jint width, jint height) {
    jbyte *bytes = (*env)->GetPrimitiveArrayCritical(env, data, NULL);
    if (bytes == NULL) return 0;
    int64_t taken = windowcast_android_host_frame(HOST(host), (uint64_t)window, (const uint8_t *)bytes,
                                                  (size_t)length, (uint64_t)durationUs, (uint32_t)width,
                                                  (uint32_t)height);
    (*env)->ReleasePrimitiveArrayCritical(env, data, bytes, JNI_ABORT);
    return taken;
}

JNIEXPORT void JNICALL
Java_dev_windowcast_host_HostNative_ended(JNIEnv *env, jclass cls, jlong host, jlong window) {
    windowcast_android_host_ended(HOST(host), (uint64_t)window);
}

JNIEXPORT jstring JNICALL
Java_dev_windowcast_host_HostNative_nextCommand(JNIEnv *env, jclass cls, jlong host, jint timeoutMs) {
    char out[COMMAND_CAP];
    int64_t n = windowcast_android_host_next_command(HOST(host), (uint32_t)timeoutMs, out, sizeof out);
    if (n <= 0) return NULL;
    return (*env)->NewStringUTF(env, out);
}
