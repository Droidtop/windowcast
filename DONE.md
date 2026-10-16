# Secure-screen helper: done and not done

Built:
- `windowcast-secure-helper` crate in the workspace (`Cargo.toml` updated).
- Binary (`main.rs`) that runs as SYSTEM, checks permission gate before any operation, logs refusal and exits if absent.
- `client` module (`src/client.rs`) with API for install, enable, disable, uninstall, and permission status.
- `permission` module (`src/permission.rs`) reading `HKLM\SOFTWARE\windowcast\SecureScreen` with stub cross-check behavior.
- `operations` module (`src/operations.rs`) with closed set (`capture_frame`, `send_input`, `secure_desktop_state`), length-checked encode/decode, and allowlist.
- `pipe` module (`src/pipe.rs`) with named pipe stub and client verification stub.
- `desktop` module (`src/desktop.rs`) with `WTSGetActiveConsoleSessionId`, `OpenInputDesktop`, `SetThreadDesktop`, and `winlogon_active` stub.
- Unit tests for permission parsing, operation codec, client-check logic (with fakes), and allowlist.
- `docs/BACKENDS.md` "Secure screen" section and `docs/SECURITY.md` threat model, quoting owner's lines verbatim.

Not built / stub / needs real Windows rig:
- Real registry read/write (stub denies; requires elevated process on Windows).
- Real named-pipe creation with DACL and client token verification (stub denies).
- Real DXGI desktop duplication capture and SendInput injection (stubs in operations).
- Real desktop switching verification (stub always returns false for `winlogon_active`).
- `run_server` loop is a sleep stub; the binary exits after the permission check.
- The scheduled task registration (`enable`) is a stub; no task is actually registered.
- No actual `secure-screen` subcommands wired into `cli-tools` or the app.
- The host integration (reserved window list entry `secure-desktop`, routing input to the helper) is described in BACKENDS.md but not implemented in host-core or agent-windows.

Checks that passed (`cargo check --target x86_64-pc-windows-gnu -p windowcast-secure-helper`, `cargo clippy -D warnings`, `cargo fmt --check`).

`cargo test` fails due to a pre-existing `windows-future` crate issue unrelated to this package.

Needs a rig check (owner's PC):
1. Install `windowcast-secure-helper` to `%ProgramFiles%\windowcast\secure-helper` with the binary.
2. Run `secure-screen enable` elevated and confirm `Enabled` DWORD and SID appear in `HKLM\SOFTWARE\windowcast\SecureScreen`.
3. Confirm `secure-screen disable` removes them.
4. Confirm the scheduled task exists after enable and is removed after disable/uninstall.
5. Confirm the named pipe `\\.\pipe\windowcast-secure` exists with DACL allowing only SYSTEM + grant SID.
6. Confirm the binary refuses any operation when the registry value is 0 or missing, logging the refusal.
7. Confirm it accepts only `CAPTURE_FRAME`, `SEND_INPUT`, `SECURE_DESKTOP_STATE` over the pipe, and verifies the client's image path is the installed agent/app binary.
8. Confirm desktop sync (`OpenInputDesktop` / `SetThreadDesktop`) works on both Default and Winlogon desktops.
9. Confirm DXGI desktop duplication delivers secure-screen pictures when the input desktop is Winlogon.
10. Confirm `SendInput` reaches the Winlogon desktop when called after `sync_to_input_desktop`.

Unsure:
- Whether the `windows` crate's `DESKTOP_ACCESS_FLAGS` newtype allows bitwise OR via `.0` (used in desktop.rs; compiles but not verified at runtime).
- Whether `HDESK` returned by `OpenInputDesktop` is directly usable with `SetThreadDesktop` (signature matches docs but untested on real Windows).
- Whether the `client` module's `install_binary` should enforce admin-only destination (stub allows any folder).
