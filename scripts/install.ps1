$ErrorActionPreference = 'Stop'
$version = $env:RAFT_COMPUTER_VERSION
if (-not $version) { throw 'RAFT_COMPUTER_VERSION is required' }
$dir = if ($env:RAFT_COMPUTER_INSTALL_DIR) { $env:RAFT_COMPUTER_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'RaftComputer' }
$id = if ($env:RAFT_COMPUTER_OPERATION_ID) { $env:RAFT_COMPUTER_OPERATION_ID } else { [guid]::NewGuid().ToString() }
$installer = if ($env:RAFT_COMPUTER_INSTALLER_BIN) { $env:RAFT_COMPUTER_INSTALLER_BIN } else { 'raft-computer-installer.exe' }
$request = @{ protocol='raft-computer-installer/v1'; installerVersion='0.1.0-rc.1'; computerVersion=$version; operation='upgrade'; operationId=$id; installDir=$dir } | ConvertTo-Json -Compress
$request | & $installer
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
