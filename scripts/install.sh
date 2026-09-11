#!/usr/bin/env sh
set -eu
: "${RAFT_COMPUTER_VERSION:?set RAFT_COMPUTER_VERSION}"
: "${RAFT_COMPUTER_INSTALL_DIR:=$HOME/.local/bin}"
: "${RAFT_COMPUTER_OPERATION_ID:=$(date +%s)-$$}"
: "${RAFT_COMPUTER_INSTALLER_VERSION:=0.1.0-rc.5}"
: "${RAFT_COMPUTER_INSTALLER_RELEASE_BASE:=https://cdn.raft.build/installer/$RAFT_COMPUTER_INSTALLER_VERSION}"
: "${RAFT_COMPUTER_INSTALLER_NODE:=node}"
: "${RAFT_COMPUTER_PRODUCT_MANIFEST_URL:=https://cdn.raft.build/computer/$RAFT_COMPUTER_VERSION/manifest.json}"
err(){ echo "[raft-computer-installer] error: $1" >&2; exit 1; }
command -v "$RAFT_COMPUTER_INSTALLER_NODE" >/dev/null 2>&1 || err "Node 24 is required (set RAFT_COMPUTER_INSTALLER_NODE)"
command -v curl >/dev/null 2>&1 || err "curl is required"
command -v mktemp >/dev/null 2>&1 || err "mktemp is required"
command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1 || err "sha256sum or shasum is required"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
base=${RAFT_COMPUTER_INSTALLER_RELEASE_BASE%/}
curl -fsSL "$base/cli.cjs" -o "$tmp/cli.cjs" || err "could not download installer"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || err "could not download installer checksum manifest"
expected=$(awk '$2=="cli.cjs" {print $1}' "$tmp/SHA256SUMS")
[ "${#expected}" -eq 64 ] || err "installer checksum manifest has no cli.cjs entry"
if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$tmp/cli.cjs" | awk '{print $1}'); else actual=$(shasum -a 256 "$tmp/cli.cjs" | awk '{print $1}'); fi
[ "$actual" = "$expected" ] || err "installer SHA-256 mismatch"
request="$tmp/request.json"
cat >"$request" <<JSON
{"protocol":"raft-computer-installer/v1","installerVersion":"$RAFT_COMPUTER_INSTALLER_VERSION","computerVersion":"$RAFT_COMPUTER_VERSION","operation":"upgrade","operationId":"$RAFT_COMPUTER_OPERATION_ID","installDir":"$RAFT_COMPUTER_INSTALL_DIR","artifactUrl":"$RAFT_COMPUTER_PRODUCT_MANIFEST_URL"}
JSON
exec "$RAFT_COMPUTER_INSTALLER_NODE" "$tmp/cli.cjs" --request "$request"
