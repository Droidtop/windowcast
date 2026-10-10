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
/* A RemoteApp launch needs the user's Windows password; the user name is in
 * the error text. */
#define WINDOWCAST_PASSWORD_NEEDED (-4)

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
/* Connects to a paired host away from the LAN by its identity (64 hex
 * digits), through Syncthing's global discovery and STUN; the host must
 * have told this client its discovery ID on an earlier session. Blocks up
 * to a minute and a half. Null on failure, with the reason in error. */
WindowcastSession *windowcast_connect_away(const WindowcastClient *client, const char *host_id,
                                           char *error, size_t error_cap);
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

/* Switches a streamed window to the carrier named backend ("Native", "Rdp",
 * ...) without a break: the new carrier arrives as a "carrier_started"
 * event (window, generation, backend, codec), to be shown beside the old
 * one and swapped to on its first picture, then
 * windowcast_session_carrier_shown; "carrier_refused" says it did not
 * happen. Returns the new generation, or WINDOWCAST_ERROR with the reason
 * in error. */
int64_t windowcast_session_switch_window(const WindowcastSession *session, uint64_t window,
                                         const char *backend, const uint32_t *codecs,
                                         size_t count, char *error, size_t error_cap);
/* Whether a window's carrier switches by itself as what it shows and the
 * connection change (on non-zero, the default) or only by hand. Automatic
 * switches arrive as "carrier_started" events, handled as for a switch by
 * hand. Returns 0 or WINDOWCAST_ERROR. */
int64_t windowcast_session_auto_switch(const WindowcastSession *session, uint32_t on);
/* The first picture of carrier `generation` was shown: it replaces the old
 * one. Returns 0 or WINDOWCAST_ERROR. */
int64_t windowcast_session_carrier_shown(const WindowcastSession *session, uint64_t window,
                                         uint32_t generation);

/* Sends one input event as JSON (serde's form of windowcast_protocol's
 * InputEvent; examples in client-core/src/ffi.rs), e.g.
 * {"Touch":{"window":7,"id":0,"x":0.5,"y":0.5,"phase":"Start"}}.
 * Pointer and touch go to the window they name (one this session
 * streams); keys (evdev codes), text and gamepads to the last such window. */
int64_t windowcast_session_send_input(const WindowcastSession *session, const char *json);
/* Gives the host this client's clipboard text; the host's changes arrive
 * as "clipboard" events. */
int64_t windowcast_session_set_clipboard(const WindowcastSession *session, const char *text);

/* Next session event as JSON, tagged by "type": windows, stream_started,
 * stream_refused, stream_stopped, window_resized, window_focused,
 * clipboard, ssh_certificate, closed.
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

/* Whether this client shows RGBA pictures (on non-zero; off by default):
 * only then are windows its rules send to RDP streamed over RDP, coming
 * out of windowcast_session_next_picture. Returns 0 or WINDOWCAST_ERROR. */
int64_t windowcast_session_accept_pictures(const WindowcastSession *session, uint32_t on);

/* Whether the dialogs, popups and menus a shown window owns are shown too,
 * each as its own window, as they open (on non-zero, the default; 0 only
 * lists them). A followed window arrives as a stream_started event for its
 * id; its "owner" and "kind" in the window list say what it belongs to.
 * Returns 0 or WINDOWCAST_ERROR. */
int64_t windowcast_session_follow_popups(const WindowcastSession *session, uint32_t on);

/* Next picture of a window streamed over RDP (stream_started with backend
 * "Rdp"): RGBA, rows from the top, width * 4 bytes each. Returns its
 * length, WINDOWCAST_TIMEOUT when the window has not changed,
 * WINDOWCAST_ENDED, or WINDOWCAST_BUFFER_TOO_SMALL (the picture stays
 * queued; *width and *height say its size). Input for that window is sent
 * as usual and goes over RDP. */
int64_t windowcast_session_next_picture(const WindowcastSession *session, uint64_t window,
                                        uint32_t timeout_ms, uint8_t *out, size_t cap,
                                        uint32_t *width, uint32_t *height);

/* The microphone, to the host's virtual microphone: start, then sound as
 * it comes (count interleaved stereo 16-bit samples at 48 kHz; encoded to
 * Opus inside), then stop. Each returns 0 or WINDOWCAST_ERROR. */
int64_t windowcast_session_start_microphone(const WindowcastSession *session);
int64_t windowcast_session_send_microphone(const WindowcastSession *session,
                                           const int16_t *samples, size_t count);
int64_t windowcast_session_stop_microphone(const WindowcastSession *session);

/* This client's ceilings for a window's stream, 0 for no limit: kept for
 * its next start and sent at once to a running one. The host adapts below
 * them and reports what it sends as "stream_quality" events. Returns 0 or
 * WINDOWCAST_ERROR. */
int64_t windowcast_session_set_stream_limits(const WindowcastSession *session, uint64_t window,
                                             uint32_t max_bitrate_kbps, uint32_t max_fps,
                                             uint32_t max_height);

/* Next Opus packet (48 kHz, stereo, 20 ms) of a window's sound. Returns its
 * length, WINDOWCAST_TIMEOUT (quiet, or no audio yet), WINDOWCAST_ENDED when
 * the window's audio is over, or WINDOWCAST_BUFFER_TOO_SMALL (the packet is
 * dropped; Opus packets are under 1500 bytes). rtp_timestamp, if not null,
 * gets the packet's 48 kHz timestamp. */
int64_t windowcast_session_next_audio(const WindowcastSession *session, uint64_t window,
                                      uint32_t timeout_ms, uint8_t *out, size_t cap,
                                      uint32_t *rtp_timestamp);

/* Account sign-in (docs/ACCOUNTS.md). */
typedef struct WindowcastOidcSignIn WindowcastOidcSignIn;

/* Asks the host at "HOST:PORT" which sign-ins it takes, without signing
 * in: JSON in out ({"host_id", "fingerprint", "trusted", "methods",
 * "providers", "kerberos_service"}). Returns its length,
 * WINDOWCAST_BUFFER_TOO_SMALL, or WINDOWCAST_ERROR with the reason in out. */
int64_t windowcast_sign_in_options(const WindowcastClient *client, const char *address,
                                   char *out, size_t cap);
/* Signs in with an account and connects. sign_in is JSON:
 * {"password":{"username":"..","password":".."}},
 * {"oidc":{"provider":"..","id_token":".."}} or "kerberos". The credential
 * goes only to a trusted host or to the one accept_host (64 hex digits,
 * may be NULL) names after the user confirmed its fingerprint; otherwise
 * this fails with an error naming the host's identity. Null on failure,
 * with the reason in error. Later connections resume with
 * windowcast_connect and no PIN while the host keeps the registration. */
WindowcastSession *windowcast_connect_account(const WindowcastClient *client,
                                              const char *address, const char *sign_in,
                                              const char *accept_host, char *error,
                                              size_t error_cap);
/* Starts an OpenID Connect sign-in in the user's browser with one of the
 * providers windowcast_sign_in_options listed (as JSON): writes the page
 * to open into url. Null on failure, with the reason in url. */
WindowcastOidcSignIn *windowcast_oidc_browser_start(const WindowcastClient *client,
                                                    const char *provider, char *url,
                                                    size_t url_cap);
/* Waits for the browser to come back and writes the ID token into token;
 * frees sign_in. Returns the token's length, WINDOWCAST_BUFFER_TOO_SMALL,
 * or WINDOWCAST_ERROR with the reason in token. */
int64_t windowcast_oidc_browser_finish(WindowcastOidcSignIn *sign_in, uint32_t timeout_ms,
                                       char *token, size_t token_cap);

typedef struct WindowcastOidcDeviceSignIn WindowcastOidcDeviceSignIn;

/* Starts an OpenID Connect sign-in the user finishes on another device
 * (the device authorization flow, RFC 8628) with one of the providers
 * windowcast_sign_in_options listed (as JSON). Writes what to show the user
 * into out as JSON: {"user_code", "verification_uri",
 * "verification_uri_complete"} (the last may be null). Null on failure,
 * with the reason in out. */
WindowcastOidcDeviceSignIn *windowcast_oidc_device_start(const WindowcastClient *client,
                                                         const char *provider, char *out,
                                                         size_t cap);
/* Waits up to timeout_ms for the user to finish. Returns the ID token's
 * length (the token in token, for the "oidc" sign-in), WINDOWCAST_TIMEOUT
 * (not yet: call again), WINDOWCAST_BUFFER_TOO_SMALL (the token is kept:
 * call again with more room), or WINDOWCAST_ERROR with the reason in token
 * (refused, or the code expired). */
int64_t windowcast_oidc_device_wait(WindowcastOidcDeviceSignIn *sign_in, uint32_t timeout_ms,
                                    char *token, size_t token_cap);
/* Frees a device sign-in, finished or not (giving up on it). */
void windowcast_oidc_device_free(WindowcastOidcDeviceSignIn *sign_in);

/* Asks the host for an SSH user certificate for public_key (an OpenSSH
 * public key line, usually windowcast_client_ssh_public_key's), for the
 * signed-in account; it arrives as an "ssh_certificate" event
 * ({"certificate", "error"}, one of them null). */
int64_t windowcast_session_request_ssh_certificate(const WindowcastSession *session,
                                                   const char *public_key);
/* This client's own SSH public key, an OpenSSH line (made on first use and
 * kept in its data folder). Returns its length or WINDOWCAST_ERROR with the
 * reason in out. */
int64_t windowcast_client_ssh_public_key(const WindowcastClient *client, char *out, size_t cap);
/* ---- The command stream (docs/COMMAND-STREAM.md; ffi_terminal.rs) ----
 * A terminal on a host of a session or on any SSH server, drawn by the
 * client library's screen model, and application launches. */

typedef struct WindowcastTerminal WindowcastTerminal;

#define WINDOWCAST_SSH_PASSWORD 0
#define WINDOWCAST_SSH_KEY 1
#define WINDOWCAST_SSH_CERTIFICATE 2 /* a host's certificate for this client's own key */
/* What to do with an SSH server whose host key is not pinned yet. A key
 * that changed is always refused. */
#define WINDOWCAST_HOSTKEY_FIRST_USE 0   /* pin what it presents */
#define WINDOWCAST_HOSTKEY_PINNED 1      /* refuse; seen_fingerprint gets its key */
#define WINDOWCAST_HOSTKEY_FINGERPRINT 2 /* accept only the given fingerprint */

/* A shell on the host of the session. NULL on failure, the reason (a
 * refusal by the host is worded for the user) in error. */
WindowcastTerminal *windowcast_session_open_terminal(const WindowcastSession *session,
                                                     uint16_t cols, uint16_t rows, char *error,
                                                     size_t error_cap);

/* Logs in to an SSH server and opens a shell. secret is the password, the
 * private key in PEM form (passphrase may be NULL), or for
 * WINDOWCAST_SSH_CERTIFICATE the certificate from an "ssh_certificate"
 * event (for windowcast_client_ssh_public_key). fingerprint is used with
 * WINDOWCAST_HOSTKEY_FINGERPRINT ("SHA256:..."). */
WindowcastTerminal *windowcast_client_ssh_terminal(
    const WindowcastClient *client, const char *host, uint16_t port, const char *user,
    int32_t auth_kind, const char *secret, const char *passphrase, int32_t host_key_policy,
    const char *fingerprint, uint16_t cols, uint16_t rows, char *error, size_t error_cap,
    char *seen_fingerprint, size_t seen_cap);

/* Ends the shell and frees the terminal. */
void windowcast_terminal_free(WindowcastTerminal *terminal);

/* Input. Each returns 0 or WINDOWCAST_ERROR. Key names: Enter Backspace Tab
 * Escape Up Down Left Right Home End PageUp PageDown Insert Delete F1..F12.
 * send_control takes a code point (a letter, or one of @[\]^_). A paste is
 * bracketed when the program asked for it. */
int64_t windowcast_terminal_send_text(const WindowcastTerminal *terminal, const char *text);
int64_t windowcast_terminal_send_key(const WindowcastTerminal *terminal, const char *name);
int64_t windowcast_terminal_send_control(const WindowcastTerminal *terminal, uint32_t code_point);
int64_t windowcast_terminal_paste(const WindowcastTerminal *terminal, const char *text);
/* The view changed size (character cells). */
int64_t windowcast_terminal_resize(const WindowcastTerminal *terminal, uint16_t cols,
                                   uint16_t rows);
/* Scroll the view back from the live screen by lines (0 returns to it). */
int64_t windowcast_terminal_scroll(const WindowcastTerminal *terminal, uint32_t lines);

/* Waits for the screen to change since seen (the version of the last
 * snapshot; 0 for anything). Returns 1 on a change, WINDOWCAST_TIMEOUT. */
int64_t windowcast_terminal_wait(const WindowcastTerminal *terminal, uint64_t seen,
                                 uint32_t timeout_ms);

/* The screen as JSON: {"cols","rows","lines":[[{"text","fg","bg","bold",
 * "italic","underline","inverse"}...]...],"cursor":[row,col]|null,
 * "alternate_screen","scrollback","version"}. fg and bg are 0xRRGGBB, or
 * null for the viewer's default. Returns the length or
 * WINDOWCAST_BUFFER_TOO_SMALL (needed gets the size). */
int64_t windowcast_terminal_snapshot(const WindowcastTerminal *terminal, uint8_t *out, size_t cap,
                                     size_t *needed);

/* Texts programs put on the clipboard (OSC 52) since the last call, as a
 * JSON array of strings. */
int64_t windowcast_terminal_take_clipboard(const WindowcastTerminal *terminal, uint8_t *out,
                                           size_t cap, size_t *needed);

/* WINDOWCAST_TIMEOUT while the shell runs; WINDOWCAST_ENDED once it ended,
 * with its exit code in code (-1 when there is none). */
int64_t windowcast_terminal_ended(const WindowcastTerminal *terminal, int32_t *code);

/* Starts an application on the host. argv_json is a JSON array of strings.
 * Its windows arrive in the window list. Returns the process id (0 if the
 * host does not know it) or WINDOWCAST_ERROR with the reason in error. A
 * program the host runs as a RemoteApp (docs/BACKENDS.md) returns 0 once it
 * started, and its windows' ids have the top bit set; it may return
 * WINDOWCAST_PASSWORD_NEEDED with the Windows user name in error, for
 * windowcast_session_launch_with_password. */
int64_t windowcast_session_launch(const WindowcastSession *session, const char *argv_json,
                                  char *error, size_t error_cap);

/* windowcast_session_launch with the Windows password the user typed for a
 * RemoteApp (password may be NULL). */
int64_t windowcast_session_launch_with_password(const WindowcastSession *session,
                                                const char *argv_json, const char *password,
                                                char *error, size_t error_cap);

#ifdef __cplusplus
}
#endif

#endif /* WINDOWCAST_H */
