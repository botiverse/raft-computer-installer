#!/bin/sh
# Download one immutable, verified native installer. All product policy lives
# in that executable. Rust's worker and supervisor share this single binary.
set -eu
# Say something before the first network round trip: the download, checksum
# and release resolution below take several sequential round trips, and a
# silent terminal reads as a hang. Status goes to stderr so stdout stays the
# installer's (including --json output).
printf 'Preparing the Raft Computer installation...\n' >&2
INSTALL_CHANNEL_DEFAULT=""
: "${RAFT_COMPUTER_INSTALLER_CHANNEL:=main}"
: "${RAFT_COMPUTER_INSTALLER_DL_BASE:=https://hands.build/dl/raft-computer-installer}"
err() { printf 'Could not start the installation: %s.\n' "$1" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || err "$1 is required"; }
# Host of an absolute URL, lowercased, without credentials or port.
host_of() { printf '%s\n' "$1" | sed -E 's#^[A-Za-z][A-Za-z0-9+.-]*://##; s#[/?\#].*$##; s#^.*@##; s#:[0-9]*$##' | tr 'A-Z' 'a-z'; }
# A URL as shown to the user: scheme, host, port and path only. Userinfo,
# query and fragment may carry credentials or signatures, so they are never
# shown; "?..." marks that something was omitted.
shown() {
  shown_prefix=; shown_rest=$1
  case "$1" in
    //*) shown_prefix=//; shown_rest=${1#//} ;;
    *://*) case "${1%%://*}" in [A-Za-z]*) case "${1%%://*}" in *[!A-Za-z0-9+.-]*) ;; *) shown_prefix=${1%%://*}://; shown_rest=${1#*://} ;; esac ;; esac ;;
  esac
  shown_authority=${shown_rest%%[/?#]*}
  shown_tail=${shown_rest#"$shown_authority"}
  shown_authority=${shown_authority##*@}
  shown_path=${shown_tail%%[?#]*}
  shown_marker=; [ "$shown_path" = "$shown_tail" ] || shown_marker='?...'
  printf '%s' "$shown_prefix$shown_authority$shown_path$shown_marker"
}
need mktemp
need uname
need awk
if command -v curl >/dev/null 2>&1; then
  dl_quiet() { curl -fsSL --connect-timeout 30 --max-time 180 "$1" -o "$2"; }
  dl_progress() { curl -fL --progress-bar --connect-timeout 30 --max-time 180 "$1" -o "$2"; }
  # Prints "<HTTP status> <Location>" of one request without following it.
  location() { curl -sS --connect-timeout 30 --max-time 30 -o /dev/null -w '%{http_code} %{redirect_url}' "$1"; }
elif command -v wget >/dev/null 2>&1; then
  dl_quiet() { wget -q --timeout=30 --tries=1 -O "$2" "$1"; }
  # GNU wget logs every URL it visits, redirect targets (signed URLs)
  # included: keep it quiet and show only its progress bar. BusyBox wget's
  # progress names the file and host only.
  case "$(wget --version 2>/dev/null || true)" in
    *'GNU Wget'*) dl_progress() { wget -q --show-progress --timeout=30 --tries=1 -O "$2" "$1"; } ;;
    *) dl_progress() { wget --timeout=30 --tries=1 -O "$2" "$1"; } ;;
  esac
  location() { wget -q --timeout=30 --tries=1 --max-redirect=0 -S -O /dev/null "$1" 2>&1 | tr -d '\r' | awk '$1 ~ /^HTTP\// {code=$2} tolower($1)=="location:" {loc=$2} END {print code, loc}'; }
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
download_pid=
trap '[ -n "$download_pid" ] && kill "$download_pid" 2>/dev/null; rm -rf "$tmp"' 0
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
  hop=$(location "$channel_url" || true)
  status=$(printf '%s\n' "$hop" | awk '{print $1}')
  release_url=$(printf '%s\n' "$hop" | awk '{print $2}')
  case "$status" in ''|000) err "could not reach $(shown "$channel_url")" ;; esac
  [ -n "$release_url" ] || err "no installation release resolves for $target (HTTP $status from $(shown "$channel_url"))"
  origin=$(printf '%s' "$base" | sed -E 's#^(https?://[^/]+).*#\1#')
  case "$release_url" in
    //*) err "invalid installation release redirect (HTTP $status to $(shown "$release_url"))" ;;
    /*) release_url="$origin$release_url" ;;
  esac
  case "$release_url" in http://*|https://*) ;; *) err "invalid installation release URL (HTTP $status to $(shown "$release_url"))" ;; esac
  # A redirect off the Hands host is a network (often a company web filter)
  # answering for Hands. Say so with the exact redirect: a retry cannot help.
  hands_host=$(host_of "$base")
  redirect_host=$(host_of "$release_url")
  if [ "$redirect_host" != "$hands_host" ]; then
    err "the network redirected $hands_host to $redirect_host (HTTP $status to $(shown "$release_url")); a company firewall or proxy may be blocking it. Ask your network administrator to allow $hands_host and *.r2.cloudflarestorage.com, then run the same command again"
  fi
  redirect="HTTP $status to $(shown "$release_url")"
  case "$release_url" in *\?*|*\#*) err "installation release redirect must not contain a query or fragment ($redirect)" ;; esac
  case "$release_url" in */releases/*/"$target") ;; *) err "installation release redirect did not freeze a release ($redirect)" ;; esac
  release_prefix=${release_url%/"$target"}
  release_id=${release_prefix##*/}
  case "$release_id" in ''|*[!a-zA-Z0-9_-]*) err "invalid immutable release identity ($redirect)" ;; esac
  case "${release_prefix%/*}" in */releases) ;; *) err "invalid immutable release path ($redirect)" ;; esac
  sums_url="$release_url?kind=sha256sums"
  binary_url="$release_url"
fi
# The checksum list and the binary are independent fetches of one frozen
# release: download them concurrently and verify once both are complete.
printf 'Downloading the installation files...\n' >&2
dl_progress "$binary_url" "$tmp/installer" & download_pid=$!
dl_quiet "$sums_url" "$tmp/SHA256SUMS" || err "could not download installation checksums from $(shown "$sums_url")"
expected=$(awk -v f="$native" '$2==f {n++; hash=$1} END {if(n==1) print tolower(hash)}' "$tmp/SHA256SUMS")
[ "${#expected}" -eq 64 ] || err "checksums must name exactly one matching installation file"
case "$expected" in *[!0-9a-f]*) err "invalid installation checksum" ;; esac
wait "$download_pid" || err "could not download the installation files from $(shown "$binary_url")"
download_pid=
[ "$(sha "$tmp/installer")" = "$expected" ] || err "installation download does not match its published checksum"
chmod 0755 "$tmp/installer"
case "${1:-}" in install|upgrade|repair|status|recover|help) cmd=$1; shift ;; *) cmd=install ;; esac
# RAFT_COMPUTER_VERSION is the long-standing pin used by the desktop/web
# install commands; forward it to the installer. An explicit --version or
# --channel argument always wins over the environment pin.
if [ -n "${RAFT_COMPUTER_VERSION:-}" ]; then
  case " $* " in *" --version"*|*" --channel"*) ;; *)
    case "$cmd" in install|upgrade|repair) set -- "$@" --version "$RAFT_COMPUTER_VERSION" ;; esac ;;
  esac
fi
if [ -n "$INSTALL_CHANNEL_DEFAULT" ]; then
  case " $* " in *" --channel"*|*" --version"*) ;; *)
    case "$cmd" in install|upgrade|repair) set -- "$@" --channel "$INSTALL_CHANNEL_DEFAULT" ;; esac ;;
  esac
fi
# Keep the shell alive so the EXIT trap removes downloads for every exit code.
code=0
"$tmp/installer" "$cmd" "$@" || code=$?
# Exit 3 means the operation could not be settled in that process: either it
# stopped part-way (a fresh process resumes it) or it never started changing
# files (a fresh process re-plans it). Both are what running the same command
# again does, so do that once here with the same verified binary; the user
# is never told to run the installer themselves. A second 3 stays 3.
if [ "$code" -eq 3 ]; then
  case "$cmd" in status|recover|help) ;; *) code=0; "$tmp/installer" "$cmd" "$@" || code=$? ;; esac
fi
exit "$code"
