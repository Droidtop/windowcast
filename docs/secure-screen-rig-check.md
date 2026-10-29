# Secure screen: what the Windows rig has to confirm

The helper's design and trust boundary are in [SECURITY.md](SECURITY.md) ("Secure screen threat model"); what it does is in the code and
its tests. This page is what an ordinary development machine cannot exercise: Winlogon and UAC capture and input need an
administrator, SYSTEM and a real console session.

## Questions only the rig answers

- Desktop Duplication currently chooses adapter 0/output 0. Confirm that
  this is the desired secure-screen display on hybrid-GPU and multi-monitor
  rigs; if not, output selection or stitching needs a follow-up.
- Confirm that the console-session SYSTEM token created with
  `SetTokenInformation(TokenSessionId)` can open both Default and Winlogon
  on supported Windows 10/11 configurations.
- Confirm Task Scheduler accepts the in-memory COM registration on the
  supported Windows versions and fires the console-connect/unlock triggers
  after boot.
- Release packaging must place `windowcast`, helper, agent and app binaries in the layout the steps below start from. (Authenticode is
  not required; SECURITY.md says why.)

## Steps

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
