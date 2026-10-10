windowcast for Linux
====================

windowcast streams application windows (not a whole desktop) from a host to a
client, each window over the protocol that suits it.

In this package
  bin/windowcast-agent-linux  the host agent: streams windows of the Wayland
                              session it runs in
  bin/windowcast-app          the reference application, a window for the host
                              or client role (or both)
  bin/windowcast-client       a command-line client: pairs, lists windows,
                              streams one and checks the frames
  bin/windowcast-testhost     a host that streams a test pattern, to try a
                              client without capturing real windows
  install.sh, uninstall.sh    per-user install without root, and its removal
  LICENSE                     GPL-3.0-only

Install (for you only, nothing needs root)
  sh install.sh                  programs in ~/.local/bin, menu entry and icon
  sh install.sh --autostart      ... and start the host agent when you sign in
  windowcast-app                 open the window; or from the application menu

Everything install.sh creates is listed in
~/.local/share/windowcast/install-manifest.

Remove it
  ~/.local/share/windowcast/uninstall.sh            keeps your pairings and identity
  ~/.local/share/windowcast/uninstall.sh --purge    deletes those too (the host
                                                    and its clients must pair again)

Without installing: run the programs in bin/ straight from this folder.

Trying it: windowcast-app --role host on one machine and windowcast-app
--role client on another, then enter the host's PIN in the client. The host
listens on TCP and UDP port 47100.

Needs a 64-bit Linux with glibc @GLIBC@ or newer. The host agent needs a Wayland
session (sway, or a compositor with the same capture protocols); the window
programs run on Wayland or X11. Its systemd user unit is
windowcast-agent.service.

https://github.com/Droidtop/windowcast
