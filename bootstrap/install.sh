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
# A fixed description of a transfer tool's exit code.
transfer_failure() {
  case "$transfer:$1" in
    curl:3) printf 'the address is not valid' ;;
    curl:6|wget:4) printf 'the host could not be resolved or reached' ;;
    curl:7) printf 'the connection failed' ;;
    curl:22|wget:8) printf 'the server returned an HTTP error' ;;
    curl:28) printf 'the request timed out' ;;
    curl:35|curl:60|wget:5) printf 'the secure connection failed; a company firewall or proxy may be intercepting it' ;;
    *:0) printf 'no HTTP response was received' ;;
    *) printf '%s exited with code %s' "$transfer" "$1" ;;
  esac
}
need mktemp
need uname
need awk
# The transfer tool's own messages never reach the terminal: curl and wget
# echo the URL they were given (credentials and query included) in some of
# their errors. Failures are reported from the exit code instead
# (transfer_failure), with every URL shown through shown().
if command -v curl >/dev/null 2>&1; then
  transfer=curl
  dl_quiet() { curl -fsL --globoff --connect-timeout 30 --max-time 180 "$1" -o "$2" 2>/dev/null; }
  # curl's progress bar and its error messages share stderr: pass on only
  # the bar's own updates ("###  42.0%"), drop everything else, and keep
  # curl's exit code.
  dl_progress() {
    { curl -fL --globoff --progress-bar --connect-timeout 30 --max-time 180 "$1" -o "$2" 2>&1 >/dev/null; echo $? > "$2.code"; } |
      awk 'BEGIN { RS = "\r" } { gsub(/\n/, ""); if ($0 ~ /^[#=O. -]*[0-9]+(\.[0-9]+)?%$/) { printf "\r%s", $0; shown = 1; fflush() } } END { if (shown) printf "\n" }' >&2
    dl_code=1; read -r dl_code < "$2.code" || true; return "$dl_code"
  }
  # Prints "<HTTP status> <Location>" of one request without following it.
  location() { curl -s --globoff --connect-timeout 30 --max-time 30 -o /dev/null -w '%{http_code} %{redirect_url}' "$1" 2>/dev/null; }
elif command -v wget >/dev/null 2>&1; then
  transfer=wget
  dl_quiet() { wget -q --timeout=30 --tries=1 -O "$2" "$1" 2>/dev/null; }
  # GNU wget -q --show-progress prints only the bar (file name, not URL) and
  # no errors; BusyBox wget's progress names the URL, so it gets none.
  case "$(wget --version 2>/dev/null || true)" in
    *'GNU Wget'*) dl_progress() { wget -q --show-progress --timeout=30 --tries=1 -O "$2" "$1"; } ;;
    *) dl_progress() { dl_quiet "$@"; } ;;
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
  hop_code=0
  hop=$(location "$channel_url") || hop_code=$?
  status=$(printf '%s\n' "$hop" | awk '{print $1}')
  release_url=$(printf '%s\n' "$hop" | awk '{print $2}')
  case "$status" in ''|000) err "could not reach $(shown "$channel_url"): $(transfer_failure "$hop_code")" ;; esac
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
sums_code=0
dl_quiet "$sums_url" "$tmp/SHA256SUMS" || sums_code=$?
[ "$sums_code" -eq 0 ] || err "could not download installation checksums from $(shown "$sums_url"): $(transfer_failure "$sums_code")"
expected=$(awk -v f="$native" '$2==f {n++; hash=$1} END {if(n==1) print tolower(hash)}' "$tmp/SHA256SUMS")
[ "${#expected}" -eq 64 ] || err "checksums must name exactly one matching installation file"
case "$expected" in *[!0-9a-f]*) err "invalid installation checksum" ;; esac
binary_code=0
wait "$download_pid" || binary_code=$?
[ "$binary_code" -eq 0 ] || err "could not download the installation files from $(shown "$binary_url"): $(transfer_failure "$binary_code")"
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
