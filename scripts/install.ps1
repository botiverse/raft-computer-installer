# raft-computer bootstrap for Windows. Downloads one pinned, verified
# installer release and hands it the request. Contains no install or upgrade
# logic of its own; what the installer does on each machine is
# https://botiverse.github.io/k-carrier/installer.html
#
#   irm https://cdn.raft.build/computer/install.ps1 | iex
#   & ([scriptblock]::Create((irm https://cdn.raft.build/computer/install.ps1))) --version 1.0.31 --yes
#   $env:RAFT_COMPUTER_NON_INTERACTIVE = 1; irm https://cdn.raft.build/computer/install.ps1 | iex
#
# Unattended runs (CI=1, RAFT_COMPUTER_NON_INTERACTIVE=1, or no console) never
# ask: no --version means the channel's current release, and running the
# installer is the consent, repair included.
$ErrorActionPreference = 'Stop'
# Publish workflows may patch this for a channel-specific copy of the script.
$InstallChannelDefault = ''
$installerVersion = if ($env:RAFT_COMPUTER_INSTALLER_VERSION) { $env:RAFT_COMPUTER_INSTALLER_VERSION } else { '0.2.0-rc.1' }
$base = if ($env:RAFT_COMPUTER_INSTALLER_RELEASE_BASE) { $env:RAFT_COMPUTER_INSTALLER_RELEASE_BASE.TrimEnd('/') } else { "https://cdn.raft.build/installer/$installerVersion" }

function Fail($message) { Write-Error "Failed before any change: $message. Nothing changed."; exit 1 }
$arch = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) { 'X64' { 'x64' } 'Arm64' { 'arm64' } default { Fail "unsupported architecture $_" } }
$platform = "win32-$arch"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("raft-computer-installer-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  $sums = Join-Path $tmp 'SHA256SUMS'
  try { Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile $sums } catch { Fail "could not download the installer checksums from $base" }
  $expected = @{}
  foreach ($line in Get-Content $sums) { if ($line -match '^([0-9a-fA-F]{64})\s+(.+)$') { $expected[$Matches[2]] = $Matches[1].ToLowerInvariant() } }
  function Fetch($name) {
    if (-not $expected.ContainsKey($name)) { Fail "installer checksums have no entry for $name" }
    $out = Join-Path $tmp ($name -replace '/', '\')
    New-Item -ItemType Directory -Force -Path (Split-Path $out) | Out-Null
    try { Invoke-WebRequest -UseBasicParsing -Uri "$base/$name" -OutFile $out } catch { Fail "could not download the installer ($name) from $base" }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $out).Hash.ToLowerInvariant()
    if ($actual -ne $expected[$name]) { Fail "installer $name does not match its published checksum" }
    return $out
  }
  $native = "native/$platform/raft-computer-installer.exe"
  if (-not $expected.ContainsKey($native)) { Fail "no single executable is published for $platform" }
  $cli = Fetch $native
  $runner = Fetch "native/$platform/raft-computer-installer-runner.exe"
  # The first word may be a command; everything else is passed through.
  $argv = @($args)
  $cmd = 'install'
  if ($argv.Count -gt 0 -and $argv[0] -in @('install', 'upgrade', 'repair', 'status', 'help')) { $cmd = $argv[0]; $argv = $argv[1..($argv.Count)] }
  if ($InstallChannelDefault -and -not ($argv -contains '--channel') -and -not ($argv -contains '--version')) { $argv += @('--channel', $InstallChannelDefault) }
  $env:RAFT_COMPUTER_INSTALLER_RUNNER = $runner
  & $cli $cmd @argv
  exit $LASTEXITCODE
} finally {
  Remove-Item -Recurse -Force -LiteralPath $tmp -ErrorAction SilentlyContinue
}
