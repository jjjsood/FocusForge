# SPDX-License-Identifier: GPL-3.0-or-later
[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$Msi, [string]$Symbols = '')

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
# msiexec rejects a relative path with forward slashes.
$Msi = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Msi)
$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"$').Matches[0].Groups[1].Value
if (-not $Symbols) { $parent = Split-Path -Parent $Msi; if (-not $parent) { $parent = (Get-Location).Path }; $Symbols = Join-Path $parent 'symbols' }
if (Test-Path (Join-Path $Symbols 'x86_64-pc-windows-msvc')) { $Symbols = Join-Path $Symbols 'x86_64-pc-windows-msvc' }
& python (Join-Path $root 'tools/package-symbols.py') verify $Symbols
if ($LASTEXITCODE -ne 0) { throw 'invalid symbol inventory' }
$logs = Join-Path $root ('.scratch/check-package-windows/' + [guid]::NewGuid().ToString())
New-Item -ItemType Directory -Force -Path $logs | Out-Null
$product = Join-Path $env:ProgramFiles 'FocusForge'
$installLog = Join-Path $logs 'install.log'
$uninstallLog = Join-Path $logs 'uninstall.log'
$shortcut = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\FocusForge\FocusForge.lnk'
$fixtureDirectory = Join-Path $logs 'fixtures'
New-Item -ItemType Directory -Force -Path $fixtureDirectory | Out-Null
Copy-Item (Join-Path $root 'fixtures/mantis-64/frames/*.tif') $fixtureDirectory
$symbolTools = Join-Path $root '.scratch/symbol-tools'
& python (Join-Path $root 'tools/package-symbols.py') install stackwalk x86_64-pc-windows-msvc $symbolTools | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'pinned stackwalker cannot run on this runner' }
function Assert-CrashRoundtrip([string]$Name, [string]$Binary, [string[]]$Arguments = @()) {
  $crash = Join-Path $logs "crashes/$Name"
  $data = Join-Path $crash 'data'; $config = Join-Path $crash 'config'; $xdg = Join-Path $crash 'xdg'
  New-Item -ItemType Directory -Force -Path $data, $config, $xdg | Out-Null
  $env:FOCUSFORGE_DATA_DIR = $data; $env:FOCUSFORGE_CONFIG_DIR = $config; $env:XDG_DATA_HOME = $xdg; $env:XDG_CONFIG_HOME = $xdg; $env:FF_FORCE_CRASH = 'segv'
  $started = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
  $process = Start-Process -FilePath $Binary -ArgumentList $Arguments -PassThru -Wait -NoNewWindow
  $status = $process.ExitCode
  Remove-Item Env:FF_FORCE_CRASH -ErrorAction SilentlyContinue
  if ($status -eq 0) { throw "$Name forced crash returned success" }
  $crashes = Join-Path $data 'focusforge/crashes'
  $dump = $null; $metadata = $null
  foreach ($unused in 1..100) {
    $dump = Get-ChildItem $crashes -Filter '*.dmp' -File -ErrorAction SilentlyContinue | Select-Object -First 1
    $metadata = Get-ChildItem $crashes -Filter '*.json' -File -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($dump -and $metadata) { break }
    Start-Sleep -Milliseconds 50
  }
  if (-not $dump) { throw "$Name crash is missing dump" }
  if (-not $metadata) { throw "$Name crash is missing metadata" }
  $rawMetadata = Get-Content $metadata.FullName -Raw
  $record = $rawMetadata | ConvertFrom-Json
  $timestamp = [regex]::Match($rawMetadata, '"timestamp"\s*:\s*"(?<value>[^"]+)"')
  if (-not $timestamp.Success -or $record.binary -ne "$Name.exe" -or $record.minidump -ne $dump.Name -or $record.pid -ne $process.Id -or $record.version -ne $version -or ([DateTimeOffset]::Parse($timestamp.Groups['value'].Value).ToUnixTimeSeconds() -lt $started - 5)) { throw "$Name crash metadata mismatch" }
  $archive = Get-ChildItem $Symbols -Filter 'FocusForge-*-x86_64-pc-windows-msvc-symbols.tar.gz' -File | Select-Object -First 1
  if (-not $archive) { throw 'Windows symbol archive is missing' }
  & python (Join-Path $root 'tools/symbolicate.py') $dump.FullName $archive.FullName | Set-Content -NoNewline (Join-Path $crash 'stackwalk.json')
  if ($LASTEXITCODE -ne 0) { throw "$Name dump did not symbolicate" }
}
# Optional-header Subsystem of a PE file: 2 is a windowed (GUI) binary, 3 a console binary.
function Get-PeSubsystem([string]$Path) {
  $bytes = [IO.File]::ReadAllBytes($Path)
  if ($bytes.Length -lt 0x40) { throw "$Path is not a PE file (too short)" }
  $pe = [BitConverter]::ToUInt32($bytes, 0x3C)
  if ($pe + 4 + 20 + 70 -gt $bytes.Length -or [BitConverter]::ToUInt32($bytes, $pe) -ne 0x00004550) { throw "$Path is not a PE file (no PE signature)" }
  [int][BitConverter]::ToUInt16($bytes, $pe + 4 + 20 + 68)
}
# DLL names in the regular import directory of a PE32+ file.
function Get-PeImports([string]$Path) {
  $bytes = [IO.File]::ReadAllBytes($Path)
  if ($bytes.Length -lt 0x40) { throw "$Path is not a PE file (too short)" }
  $pe = [BitConverter]::ToUInt32($bytes, 0x3C)
  if ($pe + 4 + 20 + 124 -gt $bytes.Length -or [BitConverter]::ToUInt32($bytes, $pe) -ne 0x00004550) { throw "$Path is not a PE file (no PE signature)" }
  $optional = $pe + 24
  if ([BitConverter]::ToUInt16($bytes, $optional) -ne 0x20B) { throw "$Path is not a PE32+ file" }
  if ([BitConverter]::ToUInt32($bytes, $optional + 108) -lt 2) { throw "$Path has no import directory" }
  $sections = $optional + [BitConverter]::ToUInt16($bytes, $pe + 20)
  $sectionCount = [BitConverter]::ToUInt16($bytes, $pe + 6)
  function ConvertTo-FileOffset([uint32]$Rva) {
    foreach ($index in 0..($sectionCount - 1)) {
      $section = $sections + 40 * $index
      $start = [BitConverter]::ToUInt32($bytes, $section + 12)
      $size = [Math]::Max([BitConverter]::ToUInt32($bytes, $section + 8), [BitConverter]::ToUInt32($bytes, $section + 16))
      if ($Rva -ge $start -and $Rva -lt $start + $size) { return [int]([BitConverter]::ToUInt32($bytes, $section + 20) + $Rva - $start) }
    }
    throw "$Path has an unmapped import RVA"
  }
  $names = @()
  $descriptor = ConvertTo-FileOffset ([BitConverter]::ToUInt32($bytes, $optional + 120))
  while ($true) {
    $nameRva = [BitConverter]::ToUInt32($bytes, $descriptor + 12)
    if ($nameRva -eq 0) { break }
    $start = ConvertTo-FileOffset $nameRva
    $end = [Array]::IndexOf($bytes, [byte]0, $start)
    $names += [Text.Encoding]::ASCII.GetString($bytes, $start, $end - $start)
    $descriptor += 20
  }
  $names
}
try {
  # msiexec is a GUI program: only a pipeline makes PowerShell wait for it and set $LASTEXITCODE.
  & msiexec.exe /i $Msi /qn /norestart '/L*v' $installLog | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "MSI install failed: $LASTEXITCODE" }
  foreach ($file in 'focusforge-cli.exe', 'focusforge-gui.exe', 'LICENSE', 'NOTICE', 'focusforge.ico') {
    if (-not (Test-Path (Join-Path $product $file))) { throw "missing installed file: $file" }
  }
  $guiSubsystem = Get-PeSubsystem (Join-Path $product 'focusforge-gui.exe'); if ($guiSubsystem -ne 2) { throw "focusforge-gui.exe must be a windowed binary (PE subsystem 2), found subsystem $guiSubsystem" }
  $cliSubsystem = Get-PeSubsystem (Join-Path $product 'focusforge-cli.exe'); if ($cliSubsystem -ne 3) { throw "focusforge-cli.exe must be a console binary (PE subsystem 3), found subsystem $cliSubsystem" }
  foreach ($exe in 'focusforge-cli.exe', 'focusforge-gui.exe') {
    $imports = @(Get-PeImports (Join-Path $product $exe))
    Write-Output "$exe imports: $($imports -join ', ')"
    $runtime = $imports | Where-Object { $_ -match '^(vcruntime|msvcp)\d*(_\d+)?\.dll$' } | Select-Object -First 1
    if ($runtime) { throw "$exe links the dynamic C runtime: $runtime" }
  }
  foreach ($file in 'LICENSE', 'NOTICE', 'focusforge.ico') { if ((Get-FileHash (Join-Path $root $file.Replace('focusforge.ico', 'assets/build/focusforge.ico'))).Hash -ne (Get-FileHash (Join-Path $product $file)).Hash) { throw "$file hash mismatch" } }
  $cli = Join-Path $product 'focusforge-cli.exe'
  $inventory = Get-Content (Join-Path $Symbols 'symbol-inventory.json') -Raw | ConvertFrom-Json
  $identity = @()
  foreach ($entry in $inventory.entries) {
    $exe = Join-Path $product ($entry.name + '.exe')
    $identity += "--identity=$($entry.name)=$exe,$(Join-Path $Symbols $entry.native)"
  }
  & python (Join-Path $root 'tools/package-symbols.py') verify-windows $identity
  if ($LASTEXITCODE -ne 0) { throw 'installed PE and archived PDB identity mismatch' }
  if (Get-ChildItem $product -Recurse -Filter '*.pdb' -File | Select-Object -First 1) { throw 'MSI installed a PDB' }
  $cliVersion = & $cli --version; if ($LASTEXITCODE -ne 0 -or $cliVersion -ne "focusforge-cli $version") { throw 'CLI version mismatch' }
  & $cli --help | Out-Null; if ($LASTEXITCODE -ne 0) { throw 'CLI help failed' }
  & python (Join-Path $root 'tools/package-cli-assets.py') verify $root $product
  if ($LASTEXITCODE -ne 0) { throw 'installed CLI assets differ' }
  $completion = Join-Path $product 'share/powershell/completions/_focusforge-cli.ps1'
  . $completion
  $input = 'focusforge-cli '
  $matches = [System.Management.Automation.CommandCompletion]::CompleteInput($input, $input.Length, $null).CompletionMatches.CompletionText
  if ($matches -notcontains 'stack') { throw 'installed PowerShell completion is not registered' }
  $info = & $cli info --json $fixtureDirectory; if ($LASTEXITCODE -ne 0) { throw 'CLI info failed' }
  $data = $info | ConvertFrom-Json; if ($data.frame_count -ne 3 -or $data.unreadable_count -ne 0 -or @($data.files).Count -ne 3 -or @($data.files | Where-Object { $_.width -ne 64 -or $_.height -ne 64 -or $_.samples_per_pixel -ne 3 -or (@($_.bits_per_sample) -join ',') -ne '16,16,16' }).Count -ne 0) { throw 'CLI info metadata mismatch' }
  & bash -c 'exec bash "$(cygpath -u "$1")" "$(cygpath -u "$2")" "$(cygpath -u "$3")"' _ (Join-Path $root 'tools/smoke-release.sh') $cli (Join-Path $logs 'smoke')
  if ($LASTEXITCODE -ne 0) { throw 'installed CLI smoke failed' }
  $launch = Join-Path $logs 'launch.png'
  $gui = Join-Path $product 'focusforge-gui.exe'
  $guiEnvironment = @{ FOCUSFORGE_DATA_DIR = (Join-Path $logs 'gui-data'); FOCUSFORGE_CONFIG_DIR = (Join-Path $logs 'gui-config'); XDG_DATA_HOME = (Join-Path $logs 'gui-cache'); XDG_CONFIG_HOME = (Join-Path $logs 'gui-config') }
  $guiPrevious = @{}
  foreach ($key in $guiEnvironment.Keys) { $guiPrevious[$key] = [Environment]::GetEnvironmentVariable($key, 'Process'); New-Item -ItemType Directory -Force -Path $guiEnvironment[$key] | Out-Null }
  try {
    foreach ($key in $guiEnvironment.Keys) { [Environment]::SetEnvironmentVariable($key, $guiEnvironment[$key], 'Process') }
    $launchArguments = '--exit-after 3000 --screenshot "' + $launch + '"'
    $launchProcess = Start-Process -FilePath $gui -ArgumentList $launchArguments -PassThru -Wait -NoNewWindow
  } finally {
    foreach ($key in $guiEnvironment.Keys) { [Environment]::SetEnvironmentVariable($key, $guiPrevious[$key], 'Process') }
  }
  if ($launchProcess.ExitCode -ne 0) { throw "installed GUI launch failed: $($launchProcess.ExitCode)" }
  & python -c 'import struct,sys; data=open(sys.argv[1], "rb").read(24); assert data[:8] == b"\x89PNG\r\n\x1a\n" and data[12:16] == b"IHDR" and all(value > 0 for value in struct.unpack(">II", data[16:24]))' $launch
  if ($LASTEXITCODE -ne 0) { throw 'installed GUI launch PNG is invalid' }
  if (-not (Test-Path $shortcut)) { throw 'missing Start-menu shortcut' }
  $link = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
  if ($link.TargetPath -ne (Join-Path $product 'focusforge-gui.exe')) { throw 'invalid Start-menu shortcut target' }
  $iconParts = $link.IconLocation -split ',', 2; $iconPath = $iconParts[0].Trim('"'); $iconIndex = if ($iconParts.Count -eq 2) { [int]$iconParts[1] } else { 0 }
  if ($iconIndex -ne 0 -or -not (Test-Path $iconPath) -or (Get-FileHash $iconPath).Hash -ne (Get-FileHash (Join-Path $root 'assets/build/focusforge.ico')).Hash) { throw 'invalid Start-menu shortcut icon' }
  $appId = (New-Object -ComObject Shell.Application).NameSpace((Split-Path $shortcut)).ParseName((Split-Path -Leaf $shortcut)).ExtendedProperty('System.AppUserModel.ID')
  Write-Output "AppUserModelID: $appId"
  if ($appId -ne 'org.focusforge.FocusForge') { throw "invalid Start-menu shortcut AppUserModelID: $appId" }
  Assert-CrashRoundtrip 'focusforge-cli' $cli @('algorithms')
  Assert-CrashRoundtrip 'focusforge-gui' (Join-Path $product 'focusforge-gui.exe')
}
finally {
  & msiexec.exe /x $Msi /qn /norestart '/L*v' $uninstallLog | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "MSI uninstall failed: $LASTEXITCODE" }
  if (Test-Path $product) { throw 'package directory remains after uninstall' }
  if (Test-Path $shortcut) { throw 'Start-menu shortcut remains after uninstall' }
}
