#!/usr/bin/env sh
# raft-computer bootstrap. Downloads one pinned, verified installer release
# and hands it the request. Contains no install or upgrade logic of its own;
# what the installer does on each machine is the contract at
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
# the installer is the consent. Repair is attended only.
set -eu
: "${RAFT_COMPUTER_INSTALLER_VERSION:=0.2.0-rc.1}"
: "${RAFT_COMPUTER_INSTALLER_RELEASE_BASE:=https://cdn.raft.build/installer/$RAFT_COMPUTER_INSTALLER_VERSION}"
err() { printf 'Failed before any change: %s. Nothing changed.\n' "$1" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || err "$1 is required"; }
need mktemp; need uname
if command -v curl >/dev/null 2>&1; then dl() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then dl() { wget -qO "$2" "$1"; }
else err "curl or wget is required"; fi
if command -v sha256sum >/dev/null 2>&1; then sha() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then sha() { shasum -a 256 "$1" | awk '{print $1}'; }
else err "sha256sum or shasum is required"; fi
case "$(uname -s)" in Darwin) plat=darwin ;; Linux) plat=linux ;; *) err "unsupported OS $(uname -s)" ;; esac
machine=$(uname -m)
if [ "$plat" = darwin ] && [ "$(/usr/sbin/sysctl -in hw.optional.arm64 2>/dev/null || true)" = 1 ]; then machine=arm64; fi
case "$machine" in arm64|aarch64) arch=arm64 ;; x86_64|amd64) arch=x64 ;; *) err "unsupported architecture $machine" ;; esac
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
base=${RAFT_COMPUTER_INSTALLER_RELEASE_BASE%/}
dl "$base/SHA256SUMS" "$tmp/SHA256SUMS" || err "could not download the installer checksums from $base"
fetch() {
  mkdir -p "$(dirname "$tmp/$1")"
  dl "$base/$1" "$tmp/$1" || err "could not download the installer ($1) from $base"
  expected=$(awk -v f="$1" '$2==f {print $1}' "$tmp/SHA256SUMS")
  [ "${#expected}" -eq 64 ] || err "installer checksums have no entry for $1"
  [ "$(sha "$tmp/$1")" = "$expected" ] || err "installer $1 does not match its published checksum"
}
# The first word may be a command; everything else is passed through.
case "${1:-}" in install|upgrade|repair|rollback|recover|status|help) cmd=$1; shift ;; *) cmd=install ;; esac
native="native/$plat-$arch/raft-computer-installer"
if [ -z "${RAFT_COMPUTER_INSTALLER_NODE:-}" ] && grep -q " $native\$" "$tmp/SHA256SUMS"; then
  # A single executable for this machine: no Node needed.
  fetch "$native"; fetch "$native-runner"
  chmod 0755 "$tmp/$native" "$tmp/$native-runner"
  RAFT_COMPUTER_INSTALLER_RUNNER="$tmp/$native-runner" exec "$tmp/$native" "$cmd" "$@"
fi
: "${RAFT_COMPUTER_INSTALLER_NODE:=node}"
need "$RAFT_COMPUTER_INSTALLER_NODE"
case "$("$RAFT_COMPUTER_INSTALLER_NODE" --version 2>/dev/null)" in
  v2[4-9].*|v[3-9][0-9].*) ;;
  *) err "no single executable is published for $plat-$arch, and Node 24 or newer is required to run the portable installer (set RAFT_COMPUTER_INSTALLER_NODE)" ;;
esac
fetch cli.cjs; fetch runner.mjs
RAFT_COMPUTER_INSTALLER_RUNNER="$tmp/runner.mjs" exec "$RAFT_COMPUTER_INSTALLER_NODE" "$tmp/cli.cjs" "$cmd" "$@"
