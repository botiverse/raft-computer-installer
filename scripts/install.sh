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
#
# Unattended runs (CI=1, RAFT_COMPUTER_NON_INTERACTIVE=1, or no terminal)
# need --version and --yes; nothing is assumed and nothing prompts.
set -eu
: "${RAFT_COMPUTER_INSTALLER_VERSION:=0.2.0-rc.1}"
: "${RAFT_COMPUTER_INSTALLER_RELEASE_BASE:=https://cdn.raft.build/installer/$RAFT_COMPUTER_INSTALLER_VERSION}"
: "${RAFT_COMPUTER_INSTALLER_NODE:=node}"
err() { printf 'Failed before any change: %s. Nothing changed.\n' "$1" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || err "$1 is required"; }
need "$RAFT_COMPUTER_INSTALLER_NODE"; need mktemp
if command -v curl >/dev/null 2>&1; then dl() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then dl() { wget -qO "$2" "$1"; }
else err "curl or wget is required"; fi
if command -v sha256sum >/dev/null 2>&1; then sha() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then sha() { shasum -a 256 "$1" | awk '{print $1}'; }
else err "sha256sum or shasum is required"; fi
case "$("$RAFT_COMPUTER_INSTALLER_NODE" --version 2>/dev/null)" in
  v2[4-9].*|v[3-9][0-9].*) ;;
  *) err "Node 24 or newer is required (set RAFT_COMPUTER_INSTALLER_NODE)" ;;
esac
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
base=${RAFT_COMPUTER_INSTALLER_RELEASE_BASE%/}
dl "$base/SHA256SUMS" "$tmp/SHA256SUMS" || err "could not download the installer checksums from $base"
for f in cli.cjs runner.mjs; do
  dl "$base/$f" "$tmp/$f" || err "could not download the installer ($f) from $base"
  expected=$(awk -v f="$f" '$2==f {print $1}' "$tmp/SHA256SUMS")
  [ "${#expected}" -eq 64 ] || err "installer checksums have no entry for $f"
  [ "$(sha "$tmp/$f")" = "$expected" ] || err "installer $f does not match its published checksum"
done
# The first word may be a command; everything else is passed through.
case "${1:-}" in install|upgrade|repair|rollback|recover|status|help) cmd=$1; shift ;; *) cmd=install ;; esac
RAFT_COMPUTER_INSTALLER_RUNNER="$tmp/runner.mjs" exec "$RAFT_COMPUTER_INSTALLER_NODE" "$tmp/cli.cjs" "$cmd" "$@"
