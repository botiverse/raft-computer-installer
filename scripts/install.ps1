$ErrorActionPreference = 'Stop'
$version = $env:RAFT_COMPUTER_VERSION
if (-not $version) { throw 'RAFT_COMPUTER_VERSION is required' }
$installerVersion = if ($env:RAFT_COMPUTER_INSTALLER_VERSION) { $env:RAFT_COMPUTER_INSTALLER_VERSION } else { '0.1.0-rc.5' }
$manifest = if ($env:RAFT_COMPUTER_PRODUCT_MANIFEST_URL) { $env:RAFT_COMPUTER_PRODUCT_MANIFEST_URL } else { "https://cdn.raft.build/computer/$version/manifest.json" }
$base = if ($env:RAFT_COMPUTER_INSTALLER_RELEASE_BASE) { $env:RAFT_COMPUTER_INSTALLER_RELEASE_BASE.TrimEnd('/') } else { "https://cdn.raft.build/installer/$installerVersion" }
$dir = if ($env:RAFT_COMPUTER_INSTALL_DIR) { $env:RAFT_COMPUTER_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'RaftComputer' }
$id = if ($env:RAFT_COMPUTER_OPERATION_ID) { $env:RAFT_COMPUTER_OPERATION_ID } else { [guid]::NewGuid().ToString() }
$tmp = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString()); New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  Invoke-WebRequest "$base/cli.cjs" -OutFile "$tmp/cli.cjs"
  Invoke-WebRequest "$base/SHA256SUMS" -OutFile "$tmp/SHA256SUMS"
  $expected = (Get-Content "$tmp/SHA256SUMS" | Where-Object { $_ -match '^([0-9a-f]{64})\s+cli\.cjs$' } | ForEach-Object { $matches[1] } | Select-Object -First 1)
  if (-not $expected) { throw 'installer checksum manifest has no cli.cjs entry' }
  $actual = (Get-FileHash "$tmp/cli.cjs" -Algorithm SHA256).Hash.ToLowerInvariant()
  if ($actual -ne $expected) { throw 'installer SHA-256 mismatch' }
  $request = @{ protocol='raft-computer-installer/v1'; installerVersion=$installerVersion; computerVersion=$version; operation='upgrade'; operationId=$id; installDir=$dir; artifactUrl=$manifest } | ConvertTo-Json -Compress
  $request | & (if ($env:RAFT_COMPUTER_INSTALLER_NODE) { $env:RAFT_COMPUTER_INSTALLER_NODE } else { 'node' }) "$tmp/cli.cjs"   if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally { Remove-Item -Recurse -Force $tmp }
