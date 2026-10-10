#!/bin/sh
# Entry point of the single-file build (windowcast-linux-<arch>.run).
# uruntime mounts the image (or unpacks it when there is no FUSE) and runs
# this script with RUNDIR set to where it is.
set -eu
PKG=${RUNDIR:?uruntime did not say where the image is}/pkg

usage() {
    cat <<'TXT'
windowcast in one file.

  ./windowcast-linux-<arch>.run                    open the window (windowcast-app)
  ./windowcast-linux-<arch>.run install [opts]     install for you: ~/.local/bin, a menu entry,
                                                   an icon (--autostart starts the host agent at sign-in)
  ./windowcast-linux-<arch>.run uninstall [opts]   remove what install put there (--purge: your data too)
  ./windowcast-linux-<arch>.run app [args]         windowcast-app
  ./windowcast-linux-<arch>.run agent [args]       windowcast-agent-linux, the Wayland host agent
  ./windowcast-linux-<arch>.run client [args]      windowcast-client, the command-line client
  ./windowcast-linux-<arch>.run testhost [args]    windowcast-testhost, a host that streams a test pattern

Run from the file itself nothing is installed. If FUSE is missing it unpacks
to a temporary folder first (or set RUNIMAGE_EXTRACT_AND_RUN=1). Options of
the runtime: --runtime-help.
TXT
}

case ${1:-} in
help | -h | --help) usage ;;
install)
    shift
    exec sh "$PKG/install.sh" "$@"
    ;;
uninstall)
    shift
    exec sh "$PKG/uninstall.sh" "$@"
    ;;
"") exec "$PKG/bin/windowcast-app" ;;
app)
    shift
    exec "$PKG/bin/windowcast-app" "$@"
    ;;
agent)
    shift
    exec "$PKG/bin/windowcast-agent-linux" "$@"
    ;;
client)
    shift
    exec "$PKG/bin/windowcast-client" "$@"
    ;;
testhost)
    shift
    exec "$PKG/bin/windowcast-testhost" "$@"
    ;;
*)
    usage >&2
    exit 2
    ;;
esac
