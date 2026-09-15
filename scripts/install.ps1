# raft-computer bootstrap for Windows. Downloads one verified installer
# release from Hands and hands it the request. Contains no install or upgrade
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
# Invoke-WebRequest renders a progress bar per chunk; on a 100 MB download that
# costs minutes. Silence it: the bootstrap prints nothing but failures anyway.
$ProgressPreference = 'SilentlyContinue'
# Publish workflows may patch this for a channel-specific copy of the script.
$InstallChannelDefault = ''
$installerChannel = if ($env:RAFT_COMPUTER_INSTALLER_CHANNEL) { $env:RAFT_COMPUTER_INSTALLER_CHANNEL } else { 'main' }
$dlBase = if ($env:RAFT_COMPUTER_INSTALLER_DL_BASE) { $env:RAFT_COMPUTER_INSTALLER_DL_BASE.TrimEnd('/') } else { 'https://hands.build/dl/raft-computer-installer' }

function Fail($message) { Write-Error "Failed before any change: $message. Nothing changed."; exit 1 }
$arch = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) { 'X64' { 'x64' } 'Arm64' { 'arm64' } default { Fail "unsupported architecture $_" } }
$platform = "win32-$arch"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("raft-computer-installer-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  # Hands hosts the bytes. One request to the channel URL answers with the
  # release-bound URL; checksums, installer and runner all come from that one
  # release, so a release changing between requests cannot mix files.
  $channelUrl = "$dlBase/$installerChannel/$platform"
  $releaseUrl = $null
  try {
    $probe = Invoke-WebRequest -UseBasicParsing -Uri $channelUrl -MaximumRedirection 0 -ErrorAction SilentlyContinue
    if ($probe.Headers.Location) { $releaseUrl = [string]$probe.Headers.Location }
  } catch {
    if ($_.Exception.Response -and $_.Exception.Response.Headers['Location']) { $releaseUrl = [string]$_.Exception.Response.Headers['Location'] }
  }
  if (-not $releaseUrl) { Fail "no installer is published for $platform on channel $installerChannel ($channelUrl)" }
  if ($releaseUrl.StartsWith('/')) { $origin = ([System.Uri]$dlBase).GetLeftPart([System.UriPartial]::Authority); $releaseUrl = "$origin$releaseUrl" }
  $sums = Join-Path $tmp 'SHA256SUMS'
  try { Invoke-WebRequest -UseBasicParsing -Uri "$releaseUrl?kind=sha256sums" -OutFile $sums } catch { Fail "could not download the installer checksums from $releaseUrl" }
  $expected = @{}
  foreach ($line in Get-Content $sums) { if ($line -match '^([0-9a-fA-F]{64})\s+(.+)$') { $expected[$Matches[2]] = $Matches[1].ToLowerInvariant() } }
  function Fetch($url, $name) {
    if (-not $expected.ContainsKey($name)) { Fail "installer checksums have no entry for $name" }
    $out = Join-Path $tmp ($name -replace '/', '\')
    New-Item -ItemType Directory -Force -Path (Split-Path $out) | Out-Null
    try { Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $out } catch { Fail "could not download the installer ($name) from $releaseUrl" }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $out).Hash.ToLowerInvariant()
    if ($actual -ne $expected[$name]) { Fail "installer $name does not match its published checksum" }
    return $out
  }
  $native = "native/$platform/raft-computer-installer.exe"
  if (-not $expected.ContainsKey($native)) { Fail "no single executable is published for $platform" }
  if (-not $expected.ContainsKey("$($native -replace '\.exe$', '')-runner.exe")) { Fail "no runner is published for $platform" }
  $cli = Fetch $releaseUrl $native
  $runner = Fetch "$releaseUrl?kind=runner" "native/$platform/raft-computer-installer-runner.exe"
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
