/*
 * windowcast client interface: the one surface every windowcast client
 * implements against (client-core/src/ffi.rs is the implementation).
 *
 * Every call blocks and may be made from any thread; nothing calls back
 * into the embedder. Strings are UTF-8 and NUL-terminated.
 *
 * SPDX-License-Identifier: GPL-3.0-only
 */
#ifndef WINDOWCAST_H
#define WINDOWCAST_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct WindowcastClient WindowcastClient;
typedef struct WindowcastSession WindowcastSession;

/* Return values of the polling calls. */
#define WINDOWCAST_TIMEOUT 0
#define WINDOWCAST_ENDED (-1)
#define WINDOWCAST_BUFFER_TOO_SMALL (-2)
#define WINDOWCAST_ERROR (-3)

/* Video codecs. */
#define WINDOWCAST_CODEC_H264 0u
#define WINDOWCAST_CODEC_H265 1u
#define WINDOWCAST_CODEC_AV1 2u

typedef struct WindowcastFrameInfo {
    uint32_t codec;         /* WINDOWCAST_CODEC_* */
    uint32_t keyframe;      /* non-zero for a keyframe */
    uint32_t rtp_timestamp; /* 90 kHz clock */
    uint64_t size;          /* frame size, also when the buffer was too small */
} WindowcastFrameInfo;

/* Opens the client identity kept in data_dir (created on first use).
 * Null on failure. */
WindowcastClient *windowcast_client_new(const char *data_dir);
/* Free every session of a client before the client. */
void windowcast_client_free(WindowcastClient *client);
/* This client's identity, 64 hex digits. */
void windowcast_client_peer_id(const WindowcastClient *client, char *out, size_t cap);

/* Connects to a host agent at "HOST:PORT": pairs with pin the first time,
 * resumes with the pinned identity when pin is NULL. Null on failure, with
 * the reason in error. */
WindowcastSession *windowcast_connect(const WindowcastClient *client, const char *address,
                                      const char *pin, char *error, size_t error_cap);
/* Closes the session (the host is told at once) and frees it. */
void windowcast_session_free(WindowcastSession *session);
/* The host's identity; returns 1 if this connection paired by PIN. */
int32_t windowcast_session_host(const WindowcastSession *session, char *out, size_t cap);

/* Asks for the window list; it arrives as a "windows" event. */
int64_t windowcast_session_request_windows(const WindowcastSession *session);
/* Asks to stream a window, decodable in the given codecs (most preferred
 * first). Answered by a "stream_started" or "stream_refused" event. */
int64_t windowcast_session_start_window(const WindowcastSession *session, uint64_t window,
                                        const uint32_t *codecs, size_t count);
int64_t windowcast_session_stop_window(const WindowcastSession *session, uint64_t window);

/* Next session event as JSON, tagged by "type": windows, stream_started,
 * stream_refused, stream_stopped, window_resized, window_focused, closed.
 * Returns its length, WINDOWCAST_TIMEOUT, or WINDOWCAST_BUFFER_TOO_SMALL
 * with the length needed in *needed (that event is lost). */
int64_t windowcast_session_next_event(const WindowcastSession *session, uint32_t timeout_ms,
                                      uint8_t *out, size_t cap, size_t *needed);

/* Next frame of a window, ready for a hardware decoder (Annex-B for
 * H.264/H.265, low-overhead OBUs for AV1). Returns its length,
 * WINDOWCAST_TIMEOUT, WINDOWCAST_ENDED when the stream is over, or
 * WINDOWCAST_BUFFER_TOO_SMALL (the frame stays queued; info->size says how
 * much room it needs). */
int64_t windowcast_session_next_frame(const WindowcastSession *session, uint64_t window,
                                      uint32_t timeout_ms, uint8_t *out, size_t cap,
                                      WindowcastFrameInfo *info);

#ifdef __cplusplus
}
#endif

#endif /* WINDOWCAST_H */
