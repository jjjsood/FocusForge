# SPDX-License-Identifier: GPL-3.0-or-later
[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$Msi)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"$').Matches[0].Groups[1].Value
$logs = Join-Path $root '.scratch/check-package-windows'
New-Item -ItemType Directory -Force -Path $logs | Out-Null
$product = Join-Path $env:ProgramFiles 'FocusForge'
$installLog = Join-Path $logs 'install.log'
$uninstallLog = Join-Path $logs 'uninstall.log'
$shortcut = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\FocusForge\FocusForge.lnk'
$fixtureDirectory = Join-Path $logs 'fixtures'
New-Item -ItemType Directory -Force -Path $fixtureDirectory | Out-Null
Copy-Item (Join-Path $root 'fixtures/mantis-64/frames/*.tif') $fixtureDirectory
try {
  & msiexec.exe /i $Msi /qn /norestart '/L*v' $installLog
  if ($LASTEXITCODE -ne 0) { throw "MSI install failed: $LASTEXITCODE" }
  foreach ($file in 'focusforge-cli.exe', 'focusforge-gui.exe', 'LICENSE', 'NOTICE', 'focusforge.ico') {
    if (-not (Test-Path (Join-Path $product $file))) { throw "missing installed file: $file" }
  }
  foreach ($file in 'LICENSE', 'NOTICE', 'focusforge.ico') { if ((Get-FileHash (Join-Path $root $file.Replace('focusforge.ico', 'assets/build/focusforge.ico'))).Hash -ne (Get-FileHash (Join-Path $product $file)).Hash) { throw "$file hash mismatch" } }
  $cli = Join-Path $product 'focusforge-cli.exe'
  $cliVersion = & $cli --version; if ($LASTEXITCODE -ne 0 -or $cliVersion -ne "focusforge-cli $version") { throw 'CLI version mismatch' }
  & $cli --help | Out-Null; if ($LASTEXITCODE -ne 0) { throw 'CLI help failed' }
  $info = & $cli info --json $fixtureDirectory; if ($LASTEXITCODE -ne 0) { throw 'CLI info failed' }
  $data = $info | ConvertFrom-Json; if ($data.frame_count -ne 3 -or $data.unreadable_count -ne 0 -or @($data.files).Count -ne 3 -or @($data.files | Where-Object { $_.width -ne 64 -or $_.height -ne 64 -or $_.samples_per_pixel -ne 3 -or (@($_.bits_per_sample) -join ',') -ne '16,16,16' }).Count -ne 0) { throw 'CLI info metadata mismatch' }
  if (-not (Test-Path $shortcut)) { throw 'missing Start-menu shortcut' }
  $link = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
  if ($link.TargetPath -ne (Join-Path $product 'focusforge-gui.exe')) { throw 'invalid Start-menu shortcut target' }
  $iconParts = $link.IconLocation -split ',', 2; $iconPath = $iconParts[0].Trim('"'); $iconIndex = if ($iconParts.Count -eq 2) { [int]$iconParts[1] } else { 0 }
  if ($iconIndex -ne 0 -or -not (Test-Path $iconPath) -or (Get-FileHash $iconPath).Hash -ne (Get-FileHash (Join-Path $root 'assets/build/focusforge.ico')).Hash) { throw 'invalid Start-menu shortcut icon' }
}
finally {
  & msiexec.exe /x $Msi /qn /norestart '/L*v' $uninstallLog
  if ($LASTEXITCODE -ne 0) { throw "MSI uninstall failed: $LASTEXITCODE" }
  if (Test-Path $product) { throw 'package directory remains after uninstall' }
  if (Test-Path $shortcut) { throw 'Start-menu shortcut remains after uninstall' }
}
