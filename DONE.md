# Secure-screen helper: implementation state

Built:

- `windows` is target-gated. The client API, protocol and tests compile
  without the Windows dependency on non-Windows targets and report that
  secure-screen support is unavailable there.
- The HKLM permission gate is real and fail-closed: `Enabled=1` and a
  syntactically valid `GrantSid` are both required. Only the elevated
  management API writes it; disable removes the key. The broker re-reads
  the grant before every operation and while idle.
- The binary requires LocalSystem, moves its fixed executable from Task
  Scheduler's Session 0 into `WTSGetActiveConsoleSessionId`, verifies that
  it is in that session, follows `OpenInputDesktop` with
  `SetThreadDesktop`, and identifies Winlogon with
  `GetUserObjectInformation(UOI_NAME)`.
- `\\.\pipe\windowcast-secure` has a protected SYSTEM + grant-SID DACL.
  Every client is checked with `GetNamedPipeClientProcessId`, its primary
  token SID, and its full image path. Only `windowcast-agent-windows.exe`
  and `windowcast-app.exe` directly under the administrator-owned
  `%ProgramFiles%\windowcast` directory pass.
- The client verifies that the pipe server is LocalSystem at the exact
  installed helper path and opens it with identification-only SQOS. It
  shares a serialized persistent connection; the broker binds clients to
  the granting user's active console session and times idle clients out.
- The length-prefixed protocol has three variants only. Requests, text and
  frame replies have hard size limits; malformed messages, unknown variants
  and empty input are refused. Tests cover the codec and allowlist.
- `capture_frame` uses DXGI Desktop Duplication and a CPU-readable D3D11
  staging texture. `send_input` accepts bounded Unicode text, absolute
  moves, left/right/middle button transitions, and Enter, Backspace, Tab,
  Escape and arrow-key transitions. Capture and input refuse unless
  Winlogon is the input desktop; `secure_desktop_state` reports that state.
- The library client API exposes install, enable, disable, uninstall,
  status and request/response operations. The existing reference app wires
  these as `windowcast-app secure-screen
  install|enable|disable|uninstall|status`; downstream applications can
  present the API in their own UI. Management is fixed to `%ProgramFiles%`,
  UAC elevation uses the `runas` verb, enable shows the unelevated account
  SID in its consent prompt, and the SYSTEM task has boot, console-connect
  and unlock triggers. Registration uses Task Scheduler COM without a Temp
  file, and protected install DACLs prevent ordinary-user replacement.
- The windowless helper logs to Windows Event Log, is per-monitor-v2 DPI
  aware, reuses Desktop Duplication, requires B8G8R8A8 capture frames, and
  maps pointer input through the captured output's desktop rectangle.
- The Windows host offers `secure-desktop` (numeric wire reservation
  `WindowId(u64::MAX)`) only while installed and enabled, encodes helper
  frames through the existing encoder stack, and routes only that entry's
  text and mouse input to the helper. It logs a plain absent/disabled
  explanation otherwise.
- Unit tests cover permission parsing, operation framing and limits,
  allowlist rejection, XML escaping, and client SID/name/folder
  verification plus server verification with fakes.

Not run or changed on this development machine:

- No helper was installed, no task was registered or started, no HKLM key
  was written, and no process was run as SYSTEM.
- Winlogon/UAC capture and input cannot be exercised by an ordinary test
  process. They require the owner's Windows rig.
- `x86_64-unknown-linux-gnu` is not installed (the only installed target is
  `x86_64-pc-windows-msvc`), so the requested Linux workspace check could
  not be run. The dependency and cfg layout is ready for it.

Validation on this checkout:

- `cargo fmt --all -- --check` passes.
- The secure-helper unit and documentation tests pass (10 unit tests), the
  protocol tests pass (17 tests), and the Windows agent library tests pass
  (14 tests).
- The helper, protocol and Windows agent pass focused clippy with warnings
  denied, and the reference app passes its Windows target check. A warning
  from the vendored `ironrdp-connector` dependency remains outside these
  changes.
- Full `cargo test --workspace` and `cargo clippy --workspace -- -D warnings`
  stop in the existing unconditional `wayland-sys` dependency on Windows:
  it imports `std::os::unix::io::RawFd` while compiling for MSVC. This is
  unrelated to secure-screen code; the focused Windows checks above pass.

Known limitations / rig questions:

- Desktop Duplication currently chooses adapter 0/output 0. Confirm that
  this is the desired secure-screen display on hybrid-GPU and multi-monitor
  rigs; if not, output selection or stitching needs a follow-up.
- Confirm that the console-session SYSTEM token created with
  `SetTokenInformation(TokenSessionId)` can open both Default and Winlogon
  on supported Windows 10/11 configurations.
- Confirm Task Scheduler accepts the in-memory COM registration on the
  supported Windows versions and fires the console-connect/unlock triggers
  after boot.
- Release packaging must place `windowcast`, helper, agent and app binaries
  in the expected layout. Authenticode is not required: for an unsigned
  install, the administrator approving UAC is the trust decision for the
  helper bytes being copied. Runtime authorization still relies on the
  protected install folder, recorded SID and exact client paths. This
  branch has no Windows installer definition to update.

Needs a rig check:

1. Build release binaries from the reviewed source. Put `windowcast-app.exe` and
   `windowcast-secure-helper.exe` together, and install the agent or
   app directly under `%ProgramFiles%\windowcast`.
2. Run `windowcast-app secure-screen install`. Confirm only
   `%ProgramFiles%\windowcast\secure-helper\windowcast-secure-helper.exe`
   is added and service mode remains disabled.
3. Run `windowcast-app secure-screen enable`, read the explanation, accept the
   UAC prompt and consent, and confirm `Enabled=1` plus the consenting
   account SID under `HKLM\SOFTWARE\windowcast\SecureScreen`.
4. Confirm Task Scheduler has `\windowcast\Secure screen helper`, principal
   LocalSystem, boot/console-connect/unlock triggers, and the fixed installed
   helper action. Confirm its broker child runs as SYSTEM in the active
   console session, not Session 0.
5. Confirm `\\.\pipe\windowcast-secure` grants only SYSTEM and the recorded
   SID. Try a same-named executable outside Program Files, a different user,
   and another executable inside the folder; all must be refused.
6. Stream `secure-desktop`, lock the PC, and raise a UAC prompt. Confirm the
   image changes between Default and Winlogon and that text, pointer moves
   and left/right/middle clicks work on both.
7. Exercise a multi-monitor and, if available, hybrid-GPU machine. Record
   which display adapter 0/output 0 selects and whether the desired secure
   screen is complete.
8. While a request is active, run `windowcast-app secure-screen disable`.
   Confirm the operation is refused, the broker exits, the task and HKLM
   grant disappear, and the host removes `secure-desktop` with a plain
   disabled message.
9. Re-enable, then run `windowcast-app secure-screen uninstall`. Confirm the
   task, grant and helper binary are gone and no SYSTEM helper remains.
