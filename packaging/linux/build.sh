#!/bin/sh
# Makes the Linux packages for one architecture from its built programs:
#
#   packaging/linux/build.sh --arch x86_64 --bins DIR --out DIR [--version V] [--release R]
#
# DIR holds the four programs built for that architecture.
# In OUT it leaves, named for the architecture and the version:
#   windowcast-linux-<arch>.tar.zst      the portable tarball
#   windowcast-linux-<arch>.run          one runnable file (uruntime + squashfs)
#   windowcast_<ver>-<rel>_<amd64|arm64>.deb
#   windowcast-<ver>-<rel>.<x86_64|aarch64>.rpm
# plus install.sh and uninstall.sh as they are, for `curl | sh`.
#
# Needs GNU tar, zstd, mksquashfs (squashfs-tools), curl and sha256sum. nfpm and
# uruntime are downloaded and checked against the hashes in pins.env.
set -eu

NAME=windowcast
BINS="windowcast-agent-linux windowcast-app windowcast-client windowcast-testhost"
DESKTOP_ID=windowcast
UNIT=windowcast-agent.service

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
# shellcheck disable=SC1091
. "$here/pins.env"

die() {
    printf 'build.sh: %s\n' "$*" >&2
    exit 1
}

ARCH=""
BINDIR=""
OUT=""
VERSION=""
RELEASE=1
while [ $# -gt 0 ]; do
    [ $# -ge 2 ] || die "$1 needs a value"
    case $1 in
    --arch) ARCH=$2 ;;
    --bins) BINDIR=$2 ;;
    --out) OUT=$2 ;;
    --version) VERSION=$2 ;;
    --release) RELEASE=$2 ;;
    *) die "unknown option $1" ;;
    esac
    shift 2
done
case $ARCH in
x86_64) NFPM_ARCH=amd64 NFPM_DL=x86_64 RPM_ARCH=x86_64 ;;
aarch64) NFPM_ARCH=arm64 NFPM_DL=arm64 RPM_ARCH=aarch64 ;;
*) die "--arch is x86_64 or aarch64" ;;
esac
[ -d "$BINDIR" ] || die "--bins DIR is needed"
[ -n "$OUT" ] || die "--out DIR is needed"
for b in $BINS; do [ -x "$BINDIR/$b" ] || die "$BINDIR/$b is missing or not executable"; done
if [ -z "$VERSION" ]; then
    VERSION=$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p}' "$root/Cargo.toml" | head -n1)
fi
[ -n "$VERSION" ] || die "no version: pass --version"
FULL=$VERSION-$RELEASE

mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
WORK=$OUT/.work-$ARCH
rm -rf -- "${WORK:?}"
mkdir -p "$WORK"
TOOLS=${TOOLS_DIR:-$OUT/.tools}
mkdir -p "$TOOLS"

# Dated by the commit, so the same sources give the same archive.
SDE=${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --format=%ct 2>/dev/null || date +%s)}

tool() { # name url sha256
    if [ ! -f "$TOOLS/$1" ]; then
        curl -fsSL --retry 3 -o "$TOOLS/$1.part" "$2"
        echo "$3  $TOOLS/$1.part" | sha256sum -c - >/dev/null || die "$1 does not match the hash in pins.env"
        mv "$TOOLS/$1.part" "$TOOLS/$1"
    fi
}

# The oldest glibc the programs run on, from the versions they ask for.
GLIBC_MIN=$(for b in $BINS; do grep -aoE 'GLIBC_[0-9]+\.[0-9]+(\.[0-9]+)?' "$BINDIR/$b"; done | sed 's/GLIBC_//' | sort -Vu | tail -n1)
[ -n "$GLIBC_MIN" ] || GLIBC_MIN=2.17

# ---- the tree every format is made from ------------------------------------
dir=$NAME-linux-$ARCH
S=$WORK/stage/$dir
mkdir -p "$S/bin" "$S/share/applications"
for b in $BINS; do install -m 755 "$BINDIR/$b" "$S/bin/$b"; done
install -m 644 "$here/$DESKTOP_ID.desktop" "$S/share/applications/$DESKTOP_ID.desktop"
mkdir -p "$S/share/systemd/user"
install -m 644 "$here/$UNIT" "$S/share/systemd/user/$UNIT"
install -m 755 "$here/install.sh" "$here/uninstall.sh" "$S/"
sed "s/@GLIBC@/$GLIBC_MIN/" "$here/README.txt" >"$S/README.txt"
chmod 644 "$S/README.txt"
install -m 644 "$root/LICENSE" "$S/LICENSE"
printf '%s\n' "$FULL" >"$S/VERSION"
chmod 644 "$S/VERSION"

# ---- 1. the portable tarball -----------------------------------------------
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$SDE" -C "$WORK/stage" -cf - "$dir" |
    zstd -19 -T0 -q -f -o "$OUT/$dir.tar.zst"

# ---- 2. one runnable file --------------------------------------------------
tool uruntime "https://github.com/VHSgunzo/uruntime/releases/download/$URUNTIME_VERSION/uruntime-runimage-squashfs-$ARCH" \
    "$(eval echo "\$URUNTIME_SHA256_$ARCH")"
R=$WORK/runroot
mkdir -p "$R/static"
cp -a "$S" "$R/pkg"
install -m 755 "$here/Run.sh" "$R/Run.sh"
# uruntime starts a RunImage through <image>/static/bash; ours only needs a
# POSIX shell, and every Linux has one.
ln -s /bin/sh "$R/static/bash"
mksquashfs "$R" "$WORK/image.sqfs" -comp zstd -noappend -all-root -quiet -no-progress -mkfs-time "$SDE" -all-time "$SDE" >/dev/null
cat "$TOOLS/uruntime" "$WORK/image.sqfs" >"$OUT/$dir.run"
chmod 755 "$OUT/$dir.run"

# ---- 3. .deb and .rpm ------------------------------------------------------
tool nfpm.tar.gz "https://github.com/goreleaser/nfpm/releases/download/v$NFPM_VERSION/nfpm_${NFPM_VERSION}_Linux_$NFPM_DL.tar.gz" \
    "$(eval echo "\$NFPM_SHA256_$ARCH")"
if [ ! -x "$TOOLS/nfpm" ]; then tar -xzf "$TOOLS/nfpm.tar.gz" -C "$TOOLS" nfpm; fi
P=$WORK/pkgfiles
mkdir -p "$P"
sed 's|@BINDIR@|/usr/bin|' "$S/share/applications/$DESKTOP_ID.desktop" >"$P/$DESKTOP_ID.desktop"
sed 's|@BINDIR@|/usr/bin|' "$here/$UNIT" >"$P/$UNIT"
# nfpm does not expand its environment in file paths, so fill the config in.
export NFPM_ARCH PKG_VERSION="$VERSION" PKG_RELEASE="$RELEASE" STAGE="$S" PKGFILES="$P" GLIBC_MIN
cp "$here/nfpm.yaml" "$WORK/nfpm.yaml"
for v in NFPM_ARCH PKG_VERSION PKG_RELEASE STAGE PKGFILES GLIBC_MIN; do
    sed -i "s|\${$v}|$(printenv "$v")|g" "$WORK/nfpm.yaml"
done
# shellcheck disable=SC2016
if grep -q '\${' "$WORK/nfpm.yaml"; then die "nfpm.yaml has a variable build.sh does not fill"; fi
"$TOOLS/nfpm" package --config "$WORK/nfpm.yaml" --packager deb --target "$OUT/${NAME}_${FULL}_$NFPM_ARCH.deb"
"$TOOLS/nfpm" package --config "$WORK/nfpm.yaml" --packager rpm --target "$OUT/$NAME-$FULL.$RPM_ARCH.rpm"

# ---- the scripts as they are, for `curl | sh` ------------------------------
install -m 755 "$here/install.sh" "$here/uninstall.sh" "$OUT/"

rm -rf -- "${WORK:?}"
# What the programs link to, for the package dependencies in nfpm.yaml.
for b in $BINS; do
    printf '%s needs:' "$b"
    readelf -d "$BINDIR/$b" | sed -n 's/.*Shared library: \[\(.*\)\]/ \1/p' | tr -d '\n'
    echo
done
printf 'Built for %s (version %s, glibc %s or newer):\n' "$ARCH" "$FULL" "$GLIBC_MIN"
ls -l "$OUT"
