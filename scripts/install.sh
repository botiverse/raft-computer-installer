#!/usr/bin/env sh
# raft-computer bootstrap. Downloads one verified installer release from
# Hands and hands it the request. Contains no install or upgrade logic of its
# own; what the installer does on each machine is the contract at
# https://botiverse.github.io/k-carrier/installer.html
#
#   curl -fsSL https://cdn.raft.build/computer/install.sh | sh
#   curl -fsSL https://cdn.raft.build/computer/install.sh | sh -s -- --channel alpha
#   curl -fsSL https://cdn.raft.build/computer/install.sh | sh -s -- --version 1.0.31 --yes
#   curl -fsSL https://cdn.raft.build/computer/install.sh | sh -s -- repair --version 1.0.31 --yes
#   CI=1 curl -fsSL https://cdn.raft.build/computer/install.sh | sh
#
# Unattended runs (CI=1, RAFT_COMPUTER_NON_INTERACTIVE=1, or no terminal)
# never ask: no --version means the channel's current release, and running
# the installer is the consent, repair included.
#
# Where the installer comes from: Hands hosts the bytes. One request to the
# channel URL answers with the release-bound URL of the current installer; the
# checksums, the installer and its runner are then all taken from that one
# release, so a release that changes between requests cannot mix files.
set -eu
# Publish workflows may patch these for a channel-specific copy of the script
# (the staging pointer sets "alpha"). An explicit --channel or --version wins
# for Computer; the installer's own channel is separate.
INSTALL_CHANNEL_DEFAULT=""
: "${RAFT_COMPUTER_INSTALLER_CHANNEL:=main}"
: "${RAFT_COMPUTER_INSTALLER_DL_BASE:=https://hands.build/dl/raft-computer-installer}"
err() { printf 'Failed before any change: %s. Nothing changed.\n' "$1" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || err "$1 is required"; }
need mktemp; need uname
if command -v curl >/dev/null 2>&1; then
  dl() { curl -fsSL "$1" -o "$2"; }
  # The Location header of the channel URL, without following it.
  location() { curl -fsS -o /dev/null -w '%{redirect_url}' "$1"; }
elif command -v wget >/dev/null 2>&1; then
  dl() { wget -qO "$2" "$1"; }
  location() { wget -q --max-redirect=0 -S -O /dev/null "$1" 2>&1 | awk 'tolower($1)=="location:" {print $2}' | tail -1 | tr -d '\r'; }
else err "curl or wget is required"; fi
if command -v sha256sum >/dev/null 2>&1; then sha() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then sha() { shasum -a 256 "$1" | awk '{print $1}'; }
else err "sha256sum or shasum is required"; fi
case "$(uname -s)" in Darwin) plat=darwin ;; Linux) plat=linux ;; *) err "unsupported OS $(uname -s)" ;; esac
machine=$(uname -m)
if [ "$plat" = darwin ] && [ "$(/usr/sbin/sysctl -in hw.optional.arm64 2>/dev/null || true)" = 1 ]; then machine=arm64; fi
case "$machine" in arm64|aarch64) arch=arm64 ;; x86_64|amd64) arch=x64 ;; *) err "unsupported architecture $machine" ;; esac
target="$plat-$arch"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
base=${RAFT_COMPUTER_INSTALLER_DL_BASE%/}
channel_url="$base/$RAFT_COMPUTER_INSTALLER_CHANNEL/$target"
release_url=$(location "$channel_url" || true)
[ -n "$release_url" ] || err "no installer is published for $target on channel $RAFT_COMPUTER_INSTALLER_CHANNEL ($channel_url)"
# A relative Location is resolved against the release server's origin.
case "$release_url" in
  /*) origin=$(printf '%s' "$base" | sed -E 's#^(https?://[^/]+).*#\1#'); release_url="$origin$release_url" ;;
esac
dl "$release_url?kind=sha256sums" "$tmp/SHA256SUMS" || err "could not download the installer checksums from $release_url"
native="native/$target/raft-computer-installer"
fetch() {
  mkdir -p "$(dirname "$tmp/$2")"
  dl "$1" "$tmp/$2" || err "could not download the installer ($2) from $release_url"
  expected=$(awk -v f="$2" '$2==f {print $1}' "$tmp/SHA256SUMS")
  [ "${#expected}" -eq 64 ] || err "installer checksums have no entry for $2"
  [ "$(sha "$tmp/$2")" = "$expected" ] || err "installer $2 does not match its published checksum"
}
# Both files must be listed before anything is fetched: a checksum file that
# is not a checksum file (or lists only one of them) stops here.
grep -q " $native\$" "$tmp/SHA256SUMS" || err "no single executable is published for $target"
grep -q " $native-runner\$" "$tmp/SHA256SUMS" || err "no runner is published for $target"
# The first word may be a command; everything else is passed through.
case "${1:-}" in install|upgrade|repair|status|help) cmd=$1; shift ;; *) cmd=install ;; esac
if [ -n "$INSTALL_CHANNEL_DEFAULT" ]; then
  case " $* " in *" --channel"*|*" --version"*) ;; *) set -- "$@" --channel "$INSTALL_CHANNEL_DEFAULT" ;; esac
fi
fetch "$release_url" "$native"
fetch "$release_url?kind=runner" "$native-runner"
chmod 0755 "$tmp/$native" "$tmp/$native-runner"
# Not exec: exec would replace this shell and the EXIT trap would never run,
# leaving ~250 MB of installer bytes in the temp directory after every run
# (that is what filled CI runners and developer disks). Run the installer as
# a child, then exit with its status so the trap cleans up.
RAFT_COMPUTER_INSTALLER_RUNNER="$tmp/$native-runner" "$tmp/$native" "$cmd" "$@"
exit $?
