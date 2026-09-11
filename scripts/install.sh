#!/usr/bin/env sh
set -eu
: "${RAFT_COMPUTER_VERSION:?set RAFT_COMPUTER_VERSION}"
: "${RAFT_COMPUTER_INSTALL_DIR:=$HOME/.local/bin}"
: "${RAFT_COMPUTER_OPERATION_ID:=$(date +%s)-$$}"
installer="${RAFT_COMPUTER_INSTALLER_BIN:-raft-computer-installer}"
request=$(mktemp); trap 'rm -f "$request"' EXIT
cat >"$request" <<JSON
{"protocol":"raft-computer-installer/v1","installerVersion":"0.1.0","computerVersion":"$RAFT_COMPUTER_VERSION","operation":"upgrade","operationId":"$RAFT_COMPUTER_OPERATION_ID","installDir":"$RAFT_COMPUTER_INSTALL_DIR"}
JSON
exec "$installer" --request "$request"
