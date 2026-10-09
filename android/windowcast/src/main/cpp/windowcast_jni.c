/*
 * JNI glue for dev.windowcast.Native: a one-to-one wrapper over the
 * windowcast C interface (client-core/include/windowcast.h). No logic of
 * its own; handles are the C pointers as jlong.
 *
 * SPDX-License-Identifier: GPL-3.0-only
 */
#include <jni.h>
#include <stdint.h>
#include <stdlib.h>

#include "windowcast.h"

#define ERROR_CAP 512
#define ID_CAP 128
#define EVENT_CAP (64 * 1024)

static __thread char last_error[ERROR_CAP];

#define CLIENT(h) ((WindowcastClient *)(intptr_t)(h))
#define SESSION(h) ((WindowcastSession *)(intptr_t)(h))

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_clientNew(JNIEnv *env, jclass cls, jstring data_dir) {
    const char *dir = (*env)->GetStringUTFChars(env, data_dir, NULL);
    WindowcastClient *client = windowcast_client_new(dir);
    (*env)->ReleaseStringUTFChars(env, data_dir, dir);
    return (jlong)(intptr_t)client;
}

JNIEXPORT void JNICALL
Java_dev_windowcast_Native_clientFree(JNIEnv *env, jclass cls, jlong client) {
    windowcast_client_free(CLIENT(client));
}

JNIEXPORT jstring JNICALL
Java_dev_windowcast_Native_clientPeerId(JNIEnv *env, jclass cls, jlong client) {
    char id[ID_CAP] = {0};
    windowcast_client_peer_id(CLIENT(client), id, sizeof id);
    return (*env)->NewStringUTF(env, id);
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_connect(JNIEnv *env, jclass cls, jlong client, jstring address,
                                   jstring pin) {
    const char *addr = (*env)->GetStringUTFChars(env, address, NULL);
    const char *pin_chars = pin ? (*env)->GetStringUTFChars(env, pin, NULL) : NULL;
    last_error[0] = 0;
    WindowcastSession *session =
        windowcast_connect(CLIENT(client), addr, pin_chars, last_error, sizeof last_error);
    if (pin_chars) (*env)->ReleaseStringUTFChars(env, pin, pin_chars);
    (*env)->ReleaseStringUTFChars(env, address, addr);
    return (jlong)(intptr_t)session;
}

JNIEXPORT jstring JNICALL
Java_dev_windowcast_Native_lastError(JNIEnv *env, jclass cls) {
    return (*env)->NewStringUTF(env, last_error);
}

JNIEXPORT void JNICALL
Java_dev_windowcast_Native_sessionFree(JNIEnv *env, jclass cls, jlong session) {
    windowcast_session_free(SESSION(session));
}

JNIEXPORT jstring JNICALL
Java_dev_windowcast_Native_sessionHost(JNIEnv *env, jclass cls, jlong session) {
    char id[ID_CAP] = {0};
    windowcast_session_host(SESSION(session), id, sizeof id);
    return (*env)->NewStringUTF(env, id);
}

JNIEXPORT jboolean JNICALL
Java_dev_windowcast_Native_sessionPaired(JNIEnv *env, jclass cls, jlong session) {
    return windowcast_session_host(SESSION(session), NULL, 0) ? JNI_TRUE : JNI_FALSE;
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_requestWindows(JNIEnv *env, jclass cls, jlong session) {
    return windowcast_session_request_windows(SESSION(session));
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_startWindow(JNIEnv *env, jclass cls, jlong session, jlong window,
                                       jintArray codecs) {
    jsize count = (*env)->GetArrayLength(env, codecs);
    jint *ids = (*env)->GetIntArrayElements(env, codecs, NULL);
    uint32_t list[8];
    size_t n = 0;
    for (jsize i = 0; i < count && n < 8; i++) list[n++] = (uint32_t)ids[i];
    (*env)->ReleaseIntArrayElements(env, codecs, ids, JNI_ABORT);
    return windowcast_session_start_window(SESSION(session), (uint64_t)window, list, n);
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_stopWindow(JNIEnv *env, jclass cls, jlong session, jlong window) {
    return windowcast_session_stop_window(SESSION(session), (uint64_t)window);
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_sendInput(JNIEnv *env, jclass cls, jlong session, jstring json) {
    const char *chars = (*env)->GetStringUTFChars(env, json, NULL);
    int64_t result = windowcast_session_send_input(SESSION(session), chars);
    (*env)->ReleaseStringUTFChars(env, json, chars);
    return result;
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_setClipboard(JNIEnv *env, jclass cls, jlong session, jstring text) {
    const char *chars = (*env)->GetStringUTFChars(env, text, NULL);
    int64_t result = windowcast_session_set_clipboard(SESSION(session), chars);
    (*env)->ReleaseStringUTFChars(env, text, chars);
    return result;
}

/* The next event as a JSON string, or null on timeout. */
JNIEXPORT jstring JNICALL
Java_dev_windowcast_Native_nextEvent(JNIEnv *env, jclass cls, jlong session, jint timeout_ms) {
    uint8_t *buf = malloc(EVENT_CAP + 1);
    if (!buf) return NULL;
    size_t needed = 0;
    int64_t len = windowcast_session_next_event(SESSION(session), (uint32_t)timeout_ms, buf,
                                                EVENT_CAP, &needed);
    jstring result = NULL;
    if (len > 0) {
        buf[len] = 0;
        result = (*env)->NewStringUTF(env, (const char *)buf);
    } else if (len == WINDOWCAST_BUFFER_TOO_SMALL || len == WINDOWCAST_ERROR) {
        result = (*env)->NewStringUTF(env, "{\"type\":\"closed\"}");
    }
    free(buf);
    return result;
}

/* Copies the next frame into a direct buffer. info gets codec, keyframe,
 * rtp timestamp and size. Returns the length or a WINDOWCAST_* status. */
JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_nextFrame(JNIEnv *env, jclass cls, jlong session, jlong window,
                                     jint timeout_ms, jobject buffer, jintArray info) {
    uint8_t *out = (*env)->GetDirectBufferAddress(env, buffer);
    jlong cap = (*env)->GetDirectBufferCapacity(env, buffer);
    if (!out || cap < 0) return WINDOWCAST_ERROR;
    WindowcastFrameInfo frame = {0};
    int64_t len = windowcast_session_next_frame(SESSION(session), (uint64_t)window,
                                                (uint32_t)timeout_ms, out, (size_t)cap, &frame);
    jint values[4] = {(jint)frame.codec, (jint)frame.keyframe, (jint)frame.rtp_timestamp,
                      (jint)frame.size};
    (*env)->SetIntArrayRegion(env, info, 0, 4, values);
    return len;
}

/* Next Opus packet of a window's sound into a direct buffer; info[0] gets
 * its RTP timestamp. Returns the length or a WINDOWCAST_* status. */
JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_nextAudio(JNIEnv *env, jclass cls, jlong session, jlong window,
                                     jint timeout_ms, jobject buffer, jintArray info) {
    uint8_t *out = (*env)->GetDirectBufferAddress(env, buffer);
    jlong cap = (*env)->GetDirectBufferCapacity(env, buffer);
    if (!out || cap < 0) return WINDOWCAST_ERROR;
    uint32_t rtp_timestamp = 0;
    int64_t len = windowcast_session_next_audio(SESSION(session), (uint64_t)window,
                                                (uint32_t)timeout_ms, out, (size_t)cap,
                                                &rtp_timestamp);
    jint values[1] = {(jint)rtp_timestamp};
    (*env)->SetIntArrayRegion(env, info, 0, 1, values);
    return len;
}

/* The microphone: start, one Opus packet from a direct buffer, stop. Each
 * returns 0 or WINDOWCAST_ERROR. */
JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_startMicrophone(JNIEnv *env, jclass cls, jlong session) {
    return windowcast_session_start_microphone(SESSION(session));
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_sendMicrophone(JNIEnv *env, jclass cls, jlong session, jobject packet,
                                          jint length) {
    const uint8_t *data = (*env)->GetDirectBufferAddress(env, packet);
    jlong cap = (*env)->GetDirectBufferCapacity(env, packet);
    if (!data || length < 0 || length > cap) return WINDOWCAST_ERROR;
    return windowcast_session_send_microphone(SESSION(session), data, (size_t)length);
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_stopMicrophone(JNIEnv *env, jclass cls, jlong session) {
    return windowcast_session_stop_microphone(SESSION(session));
}
