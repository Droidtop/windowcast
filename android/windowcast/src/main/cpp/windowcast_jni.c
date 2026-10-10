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

/* The microphone: start, sound as it comes (16-bit stereo samples), stop. Each
 * returns 0 or WINDOWCAST_ERROR. */
JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_startMicrophone(JNIEnv *env, jclass cls, jlong session) {
    return windowcast_session_start_microphone(SESSION(session));
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_sendMicrophone(JNIEnv *env, jclass cls, jlong session,
                                          jshortArray samples, jint count) {
    if (count < 0 || count > (*env)->GetArrayLength(env, samples)) return WINDOWCAST_ERROR;
    jshort *data = (*env)->GetShortArrayElements(env, samples, NULL);
    if (!data) return WINDOWCAST_ERROR;
    int64_t result = windowcast_session_send_microphone(SESSION(session), data, (size_t)count);
    (*env)->ReleaseShortArrayElements(env, samples, data, JNI_ABORT);
    return result;
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_stopMicrophone(JNIEnv *env, jclass cls, jlong session) {
    return windowcast_session_stop_microphone(SESSION(session));
}

/* Whether this client shows RGBA pictures (windows its rules send to RDP). */
JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_acceptPictures(JNIEnv *env, jclass cls, jlong session, jboolean on) {
    return windowcast_session_accept_pictures(SESSION(session), on ? 1u : 0u);
}

/* Next RGBA picture of an RDP window into a direct buffer; size[0] and
 * size[1] get its width and height. Returns the length or a WINDOWCAST_*
 * status. */
JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_nextPicture(JNIEnv *env, jclass cls, jlong session, jlong window,
                                       jint timeout_ms, jobject buffer, jintArray size) {
    uint8_t *out = (*env)->GetDirectBufferAddress(env, buffer);
    jlong cap = (*env)->GetDirectBufferCapacity(env, buffer);
    if (!out || cap < 0) return WINDOWCAST_ERROR;
    uint32_t width = 0, height = 0;
    int64_t len = windowcast_session_next_picture(SESSION(session), (uint64_t)window,
                                                  (uint32_t)timeout_ms, out, (size_t)cap, &width,
                                                  &height);
    jint values[2] = {(jint)width, (jint)height};
    (*env)->SetIntArrayRegion(env, size, 0, 2, values);
    return len;
}

/* ---- The command stream: terminals and launches (windowcast.h). ---- */

#define TERMINAL(h) ((WindowcastTerminal *)(intptr_t)(h))
#define FINGERPRINT_CAP 128

static __thread char last_fingerprint[FINGERPRINT_CAP];

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_openTerminal(JNIEnv *env, jclass cls, jlong session, jint cols,
                                        jint rows) {
    last_error[0] = 0;
    return (jlong)(intptr_t)windowcast_session_open_terminal(SESSION(session), (uint16_t)cols,
                                                            (uint16_t)rows, last_error,
                                                            sizeof last_error);
}

/* The server key an untrusted SSH server presented, from the last
 * sshTerminal call. */
JNIEXPORT jstring JNICALL
Java_dev_windowcast_Native_lastFingerprint(JNIEnv *env, jclass cls) {
    return (*env)->NewStringUTF(env, last_fingerprint);
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_sshTerminal(JNIEnv *env, jclass cls, jlong client, jstring host,
                                       jint port, jstring user, jint auth_kind, jstring secret,
                                       jstring passphrase, jint policy, jstring fingerprint,
                                       jint cols, jint rows) {
    const char *host_c = (*env)->GetStringUTFChars(env, host, NULL);
    const char *user_c = (*env)->GetStringUTFChars(env, user, NULL);
    const char *secret_c = (*env)->GetStringUTFChars(env, secret, NULL);
    const char *pass_c = passphrase ? (*env)->GetStringUTFChars(env, passphrase, NULL) : NULL;
    const char *print_c = fingerprint ? (*env)->GetStringUTFChars(env, fingerprint, NULL) : NULL;
    last_error[0] = 0;
    last_fingerprint[0] = 0;
    WindowcastTerminal *terminal = windowcast_client_ssh_terminal(
        CLIENT(client), host_c, (uint16_t)port, user_c, auth_kind, secret_c, pass_c, policy,
        print_c, (uint16_t)cols, (uint16_t)rows, last_error, sizeof last_error, last_fingerprint,
        sizeof last_fingerprint);
    if (print_c) (*env)->ReleaseStringUTFChars(env, fingerprint, print_c);
    if (pass_c) (*env)->ReleaseStringUTFChars(env, passphrase, pass_c);
    (*env)->ReleaseStringUTFChars(env, secret, secret_c);
    (*env)->ReleaseStringUTFChars(env, user, user_c);
    (*env)->ReleaseStringUTFChars(env, host, host_c);
    return (jlong)(intptr_t)terminal;
}

JNIEXPORT void JNICALL
Java_dev_windowcast_Native_terminalFree(JNIEnv *env, jclass cls, jlong terminal) {
    windowcast_terminal_free(TERMINAL(terminal));
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_terminalSendText(JNIEnv *env, jclass cls, jlong terminal,
                                            jstring text) {
    const char *chars = (*env)->GetStringUTFChars(env, text, NULL);
    int64_t result = windowcast_terminal_send_text(TERMINAL(terminal), chars);
    (*env)->ReleaseStringUTFChars(env, text, chars);
    return result;
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_terminalSendKey(JNIEnv *env, jclass cls, jlong terminal,
                                           jstring name) {
    const char *chars = (*env)->GetStringUTFChars(env, name, NULL);
    int64_t result = windowcast_terminal_send_key(TERMINAL(terminal), chars);
    (*env)->ReleaseStringUTFChars(env, name, chars);
    return result;
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_terminalSendControl(JNIEnv *env, jclass cls, jlong terminal,
                                               jint code_point) {
    return windowcast_terminal_send_control(TERMINAL(terminal), (uint32_t)code_point);
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_terminalPaste(JNIEnv *env, jclass cls, jlong terminal, jstring text) {
    const char *chars = (*env)->GetStringUTFChars(env, text, NULL);
    int64_t result = windowcast_terminal_paste(TERMINAL(terminal), chars);
    (*env)->ReleaseStringUTFChars(env, text, chars);
    return result;
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_terminalResize(JNIEnv *env, jclass cls, jlong terminal, jint cols,
                                          jint rows) {
    return windowcast_terminal_resize(TERMINAL(terminal), (uint16_t)cols, (uint16_t)rows);
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_terminalScroll(JNIEnv *env, jclass cls, jlong terminal, jint lines) {
    return windowcast_terminal_scroll(TERMINAL(terminal), (uint32_t)(lines < 0 ? 0 : lines));
}

JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_terminalWait(JNIEnv *env, jclass cls, jlong terminal, jlong seen,
                                        jint timeout_ms) {
    return windowcast_terminal_wait(TERMINAL(terminal), (uint64_t)seen, (uint32_t)timeout_ms);
}

/* A JSON text from one of the terminal calls that fill a buffer, or null. */
typedef int64_t (*fill_fn)(const WindowcastTerminal *, uint8_t *, size_t, size_t *);

static jstring terminal_text(JNIEnv *env, jlong terminal, fill_fn fill) {
    size_t cap = 64 * 1024;
    for (int attempt = 0; attempt < 3; attempt++) {
        uint8_t *buf = malloc(cap + 1);
        if (!buf) return NULL;
        size_t needed = 0;
        int64_t len = fill(TERMINAL(terminal), buf, cap, &needed);
        jstring result = NULL;
        if (len >= 0) {
            buf[len] = 0;
            result = (*env)->NewStringUTF(env, (const char *)buf);
        }
        free(buf);
        if (len >= 0) return result;
        if (len != WINDOWCAST_BUFFER_TOO_SMALL) return NULL;
        cap = needed;
    }
    return NULL;
}

JNIEXPORT jstring JNICALL
Java_dev_windowcast_Native_terminalSnapshot(JNIEnv *env, jclass cls, jlong terminal) {
    return terminal_text(env, terminal, windowcast_terminal_snapshot);
}

JNIEXPORT jstring JNICALL
Java_dev_windowcast_Native_terminalTakeClipboard(JNIEnv *env, jclass cls, jlong terminal) {
    return terminal_text(env, terminal, windowcast_terminal_take_clipboard);
}

/* Returns WINDOWCAST_TIMEOUT while the shell runs, WINDOWCAST_ENDED once it
 * ended; code[0] gets the exit code (-1 for none). */
JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_terminalEnded(JNIEnv *env, jclass cls, jlong terminal,
                                         jintArray code) {
    int32_t exit_code = -1;
    int64_t result = windowcast_terminal_ended(TERMINAL(terminal), &exit_code);
    jint value = (jint)exit_code;
    (*env)->SetIntArrayRegion(env, code, 0, 1, &value);
    return result;
}

/* Starts an application on the host. Returns its process id (0 if unknown) or
 * WINDOWCAST_ERROR with the reason in lastError. */
JNIEXPORT jlong JNICALL
Java_dev_windowcast_Native_launch(JNIEnv *env, jclass cls, jlong session, jstring argv_json) {
    const char *chars = (*env)->GetStringUTFChars(env, argv_json, NULL);
    last_error[0] = 0;
    int64_t result = windowcast_session_launch(SESSION(session), chars, last_error,
                                               sizeof last_error);
    (*env)->ReleaseStringUTFChars(env, argv_json, chars);
    return result;
}
