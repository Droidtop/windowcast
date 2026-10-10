#!/bin/sh
# Removes what install.sh put on this computer for the current user: stops
# the programs and the host agent's start at sign-in, then deletes exactly
# the files and folders listed in ~/.local/share/windowcast/install-manifest.
#
#   sh uninstall.sh [--purge]
#
# Your data (this device's identity, who it trusts, settings) stays unless
# you pass --purge. It does not touch a .deb or .rpm install; use apt or dnf for those.
#
# Works from anywhere: the copy in ~/.local/share/windowcast/, an
# unpacked tarball, or downloaded alone:
#   curl -fsSL https://github.com/Droidtop/windowcast/releases/latest/download/uninstall.sh | sh
set -eu

# ---- this project ---------------------------------------------------------
NAME=windowcast
# What install.sh --autostart creates (the host agent's unit or autostart entry).
autostart_files() {
    printf '%s\n' "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$NAME-agent.service" "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/$NAME-agent.desktop"
}
# Folders holding the person's data: the identity and trust of the host agent
# and the clients (~/.local/share/windowcast), the app's settings (~/.config/windowcast).
data_dirs() { printf '%s\n' "${XDG_CONFIG_HOME:-$HOME/.config}/$NAME" "${XDG_DATA_HOME:-$HOME/.local/share}/$NAME"; }
# The host's identity and trusted devices (~/.config/windowcast/app/host) are shared with
# droidtop-agent: --purge keeps that folder when droidtop-agent is installed here.
SHARED_WITH=droidtop-agent
shared_dir() { printf '%s
' "${XDG_CONFIG_HOME:-$HOME/.config}/windowcast/app/host"; }
shared_program_here() {
    if [ -f "${XDG_DATA_HOME:-$HOME/.local/share}/droidtop-agent/install-manifest" ]; then return 0; fi
    command -v droidtop-agent >/dev/null 2>&1 || command -v droidtop-agent-app >/dev/null 2>&1
}
# ---------------------------------------------------------------------------

TAB=$(printf '\t')

usage() {
    cat <<'EOF'
Usage: uninstall.sh [options]

Stops windowcast and removes what install.sh installed, as listed in its
install manifest. Your data is kept.

  --purge          also delete your data: this device's identity, who it
                   trusts and its settings (~/.config/windowcast and
                   ~/.local/share/windowcast); hosts and clients must pair
                   with it again. The identity folder is kept when
                   droidtop-agent is installed here and shares it
  --prefix DIR     the --prefix the install used
  --manifest FILE  read this manifest
  -h, --help       this text
EOF
}

say() { printf '%s\n' "$*"; }
die() {
    printf 'uninstall: %s\n' "$*" >&2
    exit 1
}

main() {
    PURGE=0
    PREFIX=""
    MANIFEST=""
    while [ $# -gt 0 ]; do
        case $1 in
        --purge) PURGE=1 && shift ;;
        --prefix | --manifest)
            [ $# -ge 2 ] || die "$1 needs a value"
            if [ "$1" = --prefix ]; then PREFIX=$2; else MANIFEST=$2; fi
            shift 2
            ;;
        --prefix=*) PREFIX=${1#*=} && shift ;;
        --manifest=*) MANIFEST=${1#*=} && shift ;;
        -h | --help) usage && return 0 ;;
        *) usage >&2 && die "unknown option $1" ;;
        esac
    done
    [ -n "${HOME:-}" ] || die "HOME is not set"

    if [ -z "$MANIFEST" ]; then
        if [ -n "$PREFIX" ]; then
            data_home=$PREFIX/share
        else
            data_home=${XDG_DATA_HOME:-$HOME/.local/share}
        fi
        MANIFEST=$data_home/$NAME/install-manifest
        # Run from the copy install.sh left beside its manifest.
        if [ -f "$0" ] && [ -f "$(dirname "$0")/install-manifest" ]; then
            MANIFEST=$(cd "$(dirname "$0")" && pwd)/install-manifest
        fi
    fi

    removed=0 kept=0 stopped=0
    if [ ! -f "$MANIFEST" ]; then
        say "No install manifest at $MANIFEST: install.sh has not installed $NAME here, so there is nothing to remove."
        say "(A package installed with apt or dnf is removed with apt remove $NAME or dnf remove $NAME.)"
        if [ "$PURGE" = 1 ]; then purge; fi
        return 0
    fi
    say "Removing $NAME as listed in $MANIFEST"

    work=$(mktemp -d)
    trap 'rm -rf -- "$work"' EXIT
    files=$work/files
    dirs=$work/dirs
    grep "^file$TAB" "$MANIFEST" | cut -f2- >"$files" || true
    grep "^dir$TAB" "$MANIFEST" | cut -f2- | sort -r >"$dirs" || true

    # Starting at sign-in first, then the running programs.
    autostart_files >>"$files"
    units=0
    while IFS= read -r f; do
        case $f in
        */systemd/user/*.service)
            if [ -e "$f" ]; then
                units=$((units + 1))
                if command -v systemctl >/dev/null 2>&1; then
                    systemctl --user disable --now "$(basename "$f")" >/dev/null 2>&1 || true
                fi
            fi
            ;;
        esac
    done <"$files"
    [ "$units" -eq 0 ] || say "Stopped and disabled the systemd user unit."

    stop_programs "$files"

    # The files.
    while IFS= read -r f; do
        if [ -e "$f" ] || [ -L "$f" ]; then
            rm -f -- "$f"
            say "  removed $f"
            removed=$((removed + 1))
        fi
    done <"$files"
    if [ "$units" -gt 0 ] && command -v systemctl >/dev/null 2>&1; then
        systemctl --user daemon-reload >/dev/null 2>&1 || true
    fi
    rm -f -- "$MANIFEST"
    say "  removed $MANIFEST"
    # The folders install.sh made, deepest first, only while empty.
    while IFS= read -r d; do
        if [ -d "$d" ]; then
            if rmdir "$d" 2>/dev/null; then
                say "  removed folder $d"
                removed=$((removed + 1))
            else
                say "  kept folder $d (not empty)"
                kept=$((kept + 1))
            fi
        fi
    done <"$dirs"

    if [ "$PURGE" = 1 ]; then purge; else
        say "Kept your data (identity, trusted devices, settings); --purge deletes it:"
        data_dirs | while IFS= read -r d; do
            if [ -d "$d" ]; then say "  $d"; fi
        done
        if [ -d "$(shared_dir)" ]; then say "  $(shared_dir) (this computer's identity, shared with $SHARED_WITH)"; fi
    fi
    say "Done: $removed items removed, $stopped running programs stopped."
}

# Stops the programs this install put down, found by what they run from, so
# nothing else with a similar name is touched.
stop_programs() { # file listing the installed files
    pids=
    for exe in /proc/[0-9]*/exe; do
        target=$(readlink "$exe" 2>/dev/null) || continue
        target=${target% (deleted)}
        if grep -Fxq -- "$target" "$1"; then
            pid=${exe#/proc/}
            pids="$pids ${pid%/exe}"
        fi
    done
    [ -n "$pids" ] || return 0
    # shellcheck disable=SC2086
    kill $pids 2>/dev/null || true
    n=0
    while [ $n -lt 10 ]; do
        alive=
        for p in $pids; do
            if kill -0 "$p" 2>/dev/null; then alive="$alive $p"; fi
        done
        [ -n "$alive" ] || break
        sleep 1
        n=$((n + 1))
    done
    if [ -n "${alive:-}" ]; then
        # shellcheck disable=SC2086
        kill -9 $alive 2>/dev/null || true
    fi
    for p in $pids; do
        stopped=$((stopped + 1))
        say "  stopped process $p"
    done
}

purge() {
    shared=$(shared_dir)
    shared_kept=0
    if [ -d "$shared" ] && shared_program_here; then shared_kept=1; fi
    data_dirs | while IFS= read -r d; do
        # Only a folder named for this program, never HOME or the root.
        case $d in
        /?*/"$NAME") ;;
        *) continue ;;
        esac
        if [ -d "$d" ]; then
            if [ "$shared_kept" = 1 ]; then
                delete_except "$d" "$shared"
            else
                rm -rf -- "$d"
            fi
            say "  deleted your data in $d"
        fi
    done
    if [ -d "$shared" ]; then
        if [ "$shared_kept" = 1 ]; then
            say "  kept $shared: this computer's identity and trusted devices, shared with $SHARED_WITH, which is installed"
        else
            rm -rf -- "$shared"
            # The folders above it, only if that left them empty.
            rmdir "$(dirname "$shared")" "$(dirname "$(dirname "$shared")")" 2>/dev/null || true
            say "  deleted this computer's identity and trusted devices in $shared"
        fi
    fi
}

# Everything under a folder except one path inside it (and the folders on the way to it).
delete_except() { # folder keep
    for e in "$1"/* "$1"/.[!.]*; do
        if [ ! -e "$e" ] && [ ! -L "$e" ]; then continue; fi
        if [ "$e" = "$2" ]; then continue; fi
        case $2 in
        "$e"/*) delete_except "$e" "$2" ;;
        *) rm -rf -- "$e" ;;
        esac
    done
}

main "$@"
