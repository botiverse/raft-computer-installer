# One native executable; resolve a channel once, then fetch only that release.
$ErrorActionPreference = 'Stop'
$savedProgress = $ProgressPreference
$ProgressPreference = 'SilentlyContinue'
$savedModulePath = $env:PSModulePath
$env:PSModulePath = $null
$InstallChannelDefault = ''
$channel = if ($env:RAFT_COMPUTER_INSTALLER_CHANNEL) { $env:RAFT_COMPUTER_INSTALLER_CHANNEL } else { 'main' }
$dlBase = if ($env:RAFT_COMPUTER_INSTALLER_DL_BASE) { $env:RAFT_COMPUTER_INSTALLER_DL_BASE.TrimEnd('/') } else { 'https://hands.build/dl/raft-computer-installer' }
$tmp = $null
$code = 1
function Fail($message) { throw "Could not start the installer: $message." }
function ProxyFor([Uri]$uri) {
  $exclude = if ($env:NO_PROXY) { $env:NO_PROXY } else { $env:no_proxy }
  foreach ($entry in ($exclude -split ',')) {
    $entry = $entry.Trim().ToLowerInvariant()
    if (-not $entry) { continue }
    if ($entry -eq '*') { return $null }
    $hostPart = ($entry -split ':')[0].TrimStart('.')
    if ($entry.Contains(':') -and (($entry -split ':')[-1] -ne [string]$uri.Port)) { continue }
    if ($uri.DnsSafeHost -eq $hostPart -or $uri.DnsSafeHost.EndsWith('.' + $hostPart)) { return $null }
  }
  $value = if ($uri.Scheme -eq 'https') { $env:HTTPS_PROXY } else { $env:HTTP_PROXY }
  if (-not $value) { $value = $env:ALL_PROXY }
  return $value
}
function Download($url, $out) {
  $options = @{ UseBasicParsing = $true; Uri = $url; OutFile = $out; TimeoutSec = 180 }
  $proxy = ProxyFor ([Uri]$url)
  if ($proxy) { $options.Proxy = $proxy }
  Invoke-WebRequest @options
}
try {
  $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
  if ($arch -ne 'X64') { Fail 'unsupported Windows architecture' }
  $target = 'win32-x64'
  $native = "native/$target/raft-computer-installer.exe"
  $tmp = Join-Path ([IO.Path]::GetTempPath()) ('raft-computer-installer-' + [Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $tmp | Out-Null
  if ($env:RAFT_COMPUTER_INSTALLER_RELEASE_BASE) {
    $base = $env:RAFT_COMPUTER_INSTALLER_RELEASE_BASE.TrimEnd('/')
    $sumsUrl = "$base/SHA256SUMS"
    $binaryUrl = "$base/$native"
  } else {
    if ($env:RAFT_COMPUTER_INSTALLER_VERSION) { Fail 'use INSTALLER_RELEASE_BASE for an exact static release, or INSTALLER_CHANNEL for Hands' }
    $channelUrl = "$dlBase/$channel/$target"
    $probe = [Net.HttpWebRequest]::Create($channelUrl)
    $probe.AllowAutoRedirect = $false
    $probe.Timeout = 30000
    $proxy = ProxyFor ([Uri]$channelUrl)
    if ($proxy) { $probe.Proxy = New-Object Net.WebProxy($proxy) }
    $response = $probe.GetResponse()
    try {
      $location = [string]$response.Headers['Location']
      if (-not $location) { Fail 'the installer channel did not name an immutable release' }
      $release = New-Object Uri([Uri]$channelUrl, $location)
    } finally { $response.Close() }
    if ($release.Scheme -notin @('http', 'https') -or $release.Query -or $release.Fragment -or $release.AbsoluteUri -eq $channelUrl) { Fail 'invalid installer release redirect' }
    if ($release.AbsolutePath -notmatch ('/releases/[a-zA-Z0-9_-]+/' + [Regex]::Escape($target) + '$')) { Fail 'installer release redirect did not freeze a release' }
    $releaseUrl = $release.AbsoluteUri
    $sumsUrl = "${releaseUrl}?kind=sha256sums"
    $binaryUrl = $releaseUrl
  }
  $sums = Join-Path $tmp 'SHA256SUMS'
  Download $sumsUrl $sums
  $matchesForTarget = @()
  foreach ($line in Get-Content -LiteralPath $sums) {
    if ($line -match '^([0-9a-fA-F]{64})\s+(.+)$' -and $Matches[2] -eq $native) { $matchesForTarget += $Matches[1].ToLowerInvariant() }
  }
  if ($matchesForTarget.Count -ne 1) { Fail 'checksums must name exactly one matching installer' }
  $cli = Join-Path $tmp 'installer.exe'
  Download $binaryUrl $cli
  if ((Get-FileHash -Algorithm SHA256 -LiteralPath $cli).Hash.ToLowerInvariant() -ne $matchesForTarget[0]) { Fail 'installer does not match its published checksum' }
  $argv = @($args)
  $command = 'install'
  if ($argv.Count -gt 0 -and $argv[0] -in @('install', 'upgrade', 'repair', 'status', 'recover', 'help')) {
    $command = $argv[0]
    $argv = @($argv | Select-Object -Skip 1)
  }
  if ($InstallChannelDefault -and $command -in @('install', 'upgrade', 'repair') -and -not ($argv | Where-Object { $_ -match '^--(version|channel)(=|$)' })) { $argv += @('--channel', $InstallChannelDefault) }
  & $cli $command @argv
  $code = $LASTEXITCODE
} catch {
  [Console]::Error.WriteLine('Could not start the installer: ' + $_.Exception.Message)
  $code = 1
} finally {
  if ($tmp) { Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue }
  $ProgressPreference = $savedProgress
  $env:PSModulePath = $savedModulePath
}
exit $code
