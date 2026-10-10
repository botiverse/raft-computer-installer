# One native executable; resolve a channel once, then fetch only that release.
# Pasted as `irm ... | iex` this runs in the user's own session: preferences
# changed here are restored at the end, and the script never exits that
# session (see the end of this file).
$savedErrorAction = $ErrorActionPreference
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
# The catch below prefixes "Could not start the installation: " once.
# URLs are printed in full (userinfo, query and fragment included) by product
# decision (artin, 2026-10-08): the exact URL identifies the network filter or
# mirror that answered.
function Fail($message) { throw "$message." }
# Say something before the first network round trip (see install.sh).
[Console]::Error.WriteLine('Preparing the Raft Computer installation...')
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
function Download($url, $out, [bool]$showProgress = $false) {
  $options = @{ UseBasicParsing = $true; Uri = $url; OutFile = $out; TimeoutSec = 180 }
  $proxy = ProxyFor ([Uri]$url)
  if ($proxy) { $options.Proxy = $proxy }
  $beforeDownloadProgress = $ProgressPreference
  try {
    $ProgressPreference = if ($showProgress) { 'Continue' } else { 'SilentlyContinue' }
    try { Invoke-WebRequest @options } catch { Fail ('could not download ' + $url + ': ' + $_.Exception.Message) }
  } finally {
    $ProgressPreference = $beforeDownloadProgress
  }
}
try {
  # --setup <setup arguments> (see install.sh): after a successful install, run
  # `raft-computer setup ...` with the installed binary. `irm ... | iex` cannot
  # pass arguments, so $env:RAFT_COMPUTER_SETUP carries them there
  # (whitespace-separated; it is cleared at the end so a later plain install
  # does not run setup again). Everything after --setup belongs to setup.
  $argv = @($args)
  $setupArgs = $null
  $setupAt = [Array]::IndexOf([string[]]$argv, '--setup')
  if ($setupAt -ge 0) {
    $setupArgs = @($argv | Select-Object -Skip ($setupAt + 1))
    $argv = @($argv | Select-Object -First $setupAt)
  } elseif ($env:RAFT_COMPUTER_SETUP) {
    $setupArgs = @($env:RAFT_COMPUTER_SETUP -split '\s+' | Where-Object { $_ })
  }
  if ($null -ne $setupArgs) {
    if ($setupArgs.Count -eq 0) { Fail '--setup needs a server, for example --setup /my-server' }
    if ($argv -contains '--json') { Fail '--setup cannot be combined with --json' }
    if ($argv.Count -gt 0 -and $argv[0] -in @('status', 'recover', 'help')) { Fail '--setup works only with install, upgrade or repair' }
  }
  # Read the machine architecture from the environment, never from
  # [System.Runtime.InteropServices.RuntimeInformation]: in Windows PowerShell 5.1
  # an interactive console has PSReadLine loaded, which ships a same-named stub
  # type without OSArchitecture, so the type literal resolves to the stub and
  # ::OSArchitecture is $null (report: task #877). PROCESSOR_ARCHITEW6432 names
  # the real machine when this PowerShell runs under WOW64 or x64 emulation.
  $arch = $env:PROCESSOR_ARCHITEW6432
  if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
  if ($arch -ne 'AMD64') { Fail 'unsupported Windows architecture' }
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
    try { $response = $probe.GetResponse() } catch {
      # 4xx/5xx arrive as a WebException that still carries the response.
      $webError = $_.Exception
      while ($webError -and -not ($webError -is [Net.WebException])) { $webError = $webError.InnerException }
      if (-not $webError -or -not $webError.Response) { Fail "could not reach ${channelUrl}: $($_.Exception.Message)" }
      $response = $webError.Response
    }
    try {
      $status = [int]$response.StatusCode
      $location = [string]$response.Headers['Location']
    } finally { $response.Close() }
    if (-not $location) { Fail "the installation channel did not name an immutable release (HTTP $status from $channelUrl)" }
    $release = New-Object Uri([Uri]$channelUrl, $location)
    $redirect = "HTTP $status to " + $release.AbsoluteUri
    if ($release.Scheme -notin @('http', 'https')) { Fail "invalid installation release redirect ($redirect)" }
    # A redirect off the Hands host is a network (often a company web filter)
    # answering for Hands. Say so with the exact redirect: a retry cannot help.
    $handsHost = ([Uri]$channelUrl).Host
    if ($release.Host -ne $handsHost) { Fail "the network redirected the request for $channelUrl (HTTP $status) to $($release.AbsoluteUri); a company firewall or proxy may be blocking $handsHost. Ask your network administrator to allow $handsHost and *.r2.cloudflarestorage.com, then run the same command again" }
    if ($release.Query -or $release.Fragment -or $release.AbsoluteUri -eq $channelUrl) { Fail "invalid installation release redirect ($redirect)" }
    if ($release.AbsolutePath -notmatch ('/releases/[a-zA-Z0-9_-]+/' + [Regex]::Escape($target) + '$')) { Fail "installation release redirect did not freeze a release ($redirect)" }
    $releaseUrl = $release.AbsoluteUri
    $sumsUrl = "${releaseUrl}?kind=sha256sums"
    $binaryUrl = $releaseUrl
  }
  $sums = Join-Path $tmp 'SHA256SUMS'
  Download $sumsUrl $sums $false
  $matchesForTarget = @()
  foreach ($line in Get-Content -LiteralPath $sums) {
    if ($line -match '^([0-9a-fA-F]{64})\s+(.+)$' -and $Matches[2] -eq $native) { $matchesForTarget += $Matches[1].ToLowerInvariant() }
  }
  if ($matchesForTarget.Count -ne 1) { Fail 'checksums must name exactly one matching installation file' }
  [Console]::Error.WriteLine('Downloading the installation files...')
  $cli = Join-Path $tmp 'installer.exe'
  Download $binaryUrl $cli $true
  if ((Get-FileHash -Algorithm SHA256 -LiteralPath $cli).Hash.ToLowerInvariant() -ne $matchesForTarget[0]) { Fail 'installation download does not match its published checksum' }
  $command = 'install'
  if ($argv.Count -gt 0 -and $argv[0] -in @('install', 'upgrade', 'repair', 'status', 'recover', 'help')) {
    $command = $argv[0]
    $argv = @($argv | Select-Object -Skip 1)
  }
  # RAFT_COMPUTER_VERSION is the long-standing pin used by the desktop/web
  # install commands; forward it to the installer. An explicit --version or
  # --channel argument always wins over the environment pin.
  if ($env:RAFT_COMPUTER_VERSION -and $command -in @('install', 'upgrade', 'repair') -and -not ($argv | Where-Object { $_ -match '^--(version|channel)(=|$)' })) { $argv += @('--version', $env:RAFT_COMPUTER_VERSION) }
  if ($InstallChannelDefault -and $command -in @('install', 'upgrade', 'repair') -and -not ($argv | Where-Object { $_ -match '^--(version|channel)(=|$)' })) { $argv += @('--channel', $InstallChannelDefault) }
  # Same resolution as the native config: binary, else install dir, else
  # ~\.local\bin; a leading ~ or a relative path is under the user's home.
  $userHome = $env:USERPROFILE
  function UnderHome($path) {
    if ($path -eq '~') { return $userHome }
    if ($path -match '^~[\\/]') { return Join-Path $userHome $path.Substring(2) }
    if ([IO.Path]::IsPathRooted($path)) { return $path }
    return Join-Path $userHome $path
  }
  $installed = if ($env:RAFT_COMPUTER_BINARY) { UnderHome $env:RAFT_COMPUTER_BINARY } else {
    $dir = if ($env:RAFT_COMPUTER_INSTALL_DIR) { UnderHome $env:RAFT_COMPUTER_INSTALL_DIR } else { Join-Path $userHome '.local\bin' }
    Join-Path $dir 'raft-computer.exe'
  }
  $installedDir = Split-Path -Parent $installed
  # Pasted (`irm … | iex`) this script runs in the user's own session, so it
  # can put the install directory on that session's Path after the native
  # installer adds it to the user Path (which it does only for the default
  # directory). Run as a file the session is a throwaway process: skip it.
  $pastedSession = $true
  try { if ($PSCommandPath) { $pastedSession = $false } } catch { }
  $sessionPath = $pastedSession -and $command -in @('install', 'upgrade', 'repair') -and
    $env:RAFT_COMPUTER_NO_MODIFY_PATH -ne '1' -and
    $installedDir.TrimEnd('\') -ieq (Join-Path $userHome '.local\bin').TrimEnd('\')
  if ($sessionPath) { $env:RAFT_COMPUTER_SESSION_PATH = '1' }
  & $cli $command @argv
  $code = $LASTEXITCODE
  # Exit 3 means the operation could not be settled in that process: either it
  # stopped part-way (a fresh process resumes it) or it never started changing
  # files (a fresh process re-plans it). Both are what running the same command
  # again does, so do that once here with the same verified binary; the user
  # is never told to run the installer themselves. A second 3 stays 3.
  if ($code -eq 3 -and $command -notin @('status', 'recover', 'help')) {
    & $cli $command @argv
    $code = $LASTEXITCODE
  }
  if ($code -eq 0 -and $sessionPath) {
    # The new user Path reaches new windows only; give this session the same
    # entry, as the native installer can not (it is a child process).
    if (-not (($env:Path -split ';') | Where-Object { $_.TrimEnd('\') -ieq $installedDir.TrimEnd('\') })) {
      $env:Path = if ($env:Path) { "$installedDir;$env:Path" } else { $installedDir }
    }
  }
  if ($code -eq 0 -and $null -ne $setupArgs) {
    Write-Host '==> Setting up Raft Computer'
    & $installed setup @setupArgs
    $code = $LASTEXITCODE
  }
} catch {
  [Console]::Error.WriteLine('Could not start the installation: ' + $_.Exception.Message)
  $code = 1
} finally {
  Remove-Item Env:RAFT_COMPUTER_SETUP -ErrorAction SilentlyContinue
  Remove-Item Env:RAFT_COMPUTER_SESSION_PATH -ErrorAction SilentlyContinue
  if ($tmp) { Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue }
  $ProgressPreference = $savedProgress
  $env:PSModulePath = $savedModulePath
  $ErrorActionPreference = $savedErrorAction
}
# Run as a file (powershell -File install.ps1, or & .\install.ps1) the code
# is the process exit code, as automation expects. Pasted as
# `irm ... | iex` or run as `& ([scriptblock]::Create((irm ...)))` there is no
# script file, and exit would end the user's PowerShell session: the window
# closes and the message above vanishes with it. Report the code the way a
# native command does instead; nothing follows in this script.
$runAsFile = $false
try { $runAsFile = [bool]$PSCommandPath } catch { }
if ($runAsFile) { exit $code }
$global:LASTEXITCODE = $code
