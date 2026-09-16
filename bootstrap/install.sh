#!/bin/sh
# Download one immutable, verified native installer. All product policy lives
# in that executable. Rust's worker and supervisor share this single binary.
set -eu
INSTALL_CHANNEL_DEFAULT=""
: "${RAFT_COMPUTER_INSTALLER_CHANNEL:=main}"
: "${RAFT_COMPUTER_INSTALLER_DL_BASE:=https://hands.build/dl/raft-computer-installer}"
err() { printf 'Could not start the installer: %s.\n' "$1" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || err "$1 is required"; }
need mktemp
need uname
need awk
if command -v curl >/dev/null 2>&1; then
  dl() { curl -fsSL --connect-timeout 30 --max-time 180 "$1" -o "$2"; }
  location() { curl -fsS --connect-timeout 30 --max-time 30 -o /dev/null -w '%{redirect_url}' "$1"; }
elif command -v wget >/dev/null 2>&1; then
  dl() { wget -q --timeout=30 --tries=1 -O "$2" "$1"; }
  location() { wget -q --timeout=30 --tries=1 --max-redirect=0 -S -O /dev/null "$1" 2>&1 | awk 'tolower($1)=="location:" {print $2}' | tail -1 | tr -d '\r'; }
else err "curl or wget is required"; fi
if command -v sha256sum >/dev/null 2>&1; then sha() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then sha() { shasum -a 256 "$1" | awk '{print $1}'; }
else err "sha256sum or shasum is required"; fi
case "$(uname -s)" in Darwin) plat=darwin ;; Linux) plat=linux ;; *) err "unsupported operating system" ;; esac
machine=$(uname -m)
if [ "$plat" = darwin ] && [ "$(/usr/sbin/sysctl -in hw.optional.arm64 2>/dev/null || true)" = 1 ]; then machine=arm64; fi
case "$machine" in arm64|aarch64) arch=arm64 ;; x86_64|amd64) arch=x64 ;; *) err "unsupported architecture" ;; esac
target="$plat-$arch"
native="native/$target/raft-computer-installer"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' 0
trap 'exit 130' 2
trap 'exit 143' 15
if [ -n "${RAFT_COMPUTER_INSTALLER_RELEASE_BASE:-}" ]; then
  # Explicit static mirror: caller names the complete immutable version base.
  base=${RAFT_COMPUTER_INSTALLER_RELEASE_BASE%/}
  sums_url="$base/SHA256SUMS"
  binary_url="$base/$native"
else
  [ -z "${RAFT_COMPUTER_INSTALLER_VERSION:-}" ] || err "use INSTALLER_RELEASE_BASE for an exact static release, or INSTALLER_CHANNEL for Hands"
  base=${RAFT_COMPUTER_INSTALLER_DL_BASE%/}
  channel_url="$base/$RAFT_COMPUTER_INSTALLER_CHANNEL/$target"
  release_url=$(location "$channel_url" || true)
  [ -n "$release_url" ] || err "no installer release resolves for $target"
  case "$release_url" in
    //*) err "invalid installer release redirect" ;;
    /*) origin=$(printf '%s' "$base" | sed -E 's#^(https?://[^/]+).*#\1#'); release_url="$origin$release_url" ;;
  esac
  case "$release_url" in http://*|https://*) ;; *) err "invalid installer release URL" ;; esac
  case "$release_url" in *\?*|*\#*) err "installer release redirect must not contain a query or fragment" ;; esac
  case "$release_url" in */releases/*/"$target") ;; *) err "installer release redirect did not freeze a release" ;; esac
  release_prefix=${release_url%/"$target"}
  release_id=${release_prefix##*/}
  case "$release_id" in ''|*[!a-zA-Z0-9_-]*) err "invalid immutable release identity" ;; esac
  case "${release_prefix%/*}" in */releases) ;; *) err "invalid immutable release path" ;; esac
  sums_url="$release_url?kind=sha256sums"
  binary_url="$release_url"
fi
dl "$sums_url" "$tmp/SHA256SUMS" || err "could not download installer checksums"
expected=$(awk -v f="$native" '$2==f {n++; hash=$1} END {if(n==1) print tolower(hash)}' "$tmp/SHA256SUMS")
[ "${#expected}" -eq 64 ] || err "checksums must name exactly one matching installer"
case "$expected" in *[!0-9a-f]*) err "invalid installer checksum" ;; esac
dl "$binary_url" "$tmp/installer" || err "could not download installer"
[ "$(sha "$tmp/installer")" = "$expected" ] || err "installer does not match its published checksum"
chmod 0755 "$tmp/installer"
case "${1:-}" in install|upgrade|repair|status|recover|help) cmd=$1; shift ;; *) cmd=install ;; esac
if [ -n "$INSTALL_CHANNEL_DEFAULT" ]; then
  case " $* " in *" --channel"*|*" --version"*) ;; *)
    case "$cmd" in install|upgrade|repair) set -- "$@" --channel "$INSTALL_CHANNEL_DEFAULT" ;; esac ;;
  esac
fi
# Keep the shell alive so the EXIT trap removes downloads for every exit code.
code=0
"$tmp/installer" "$cmd" "$@" || code=$?
exit "$code"
