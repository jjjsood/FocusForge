# SPDX-License-Identifier: GPL-3.0-or-later
[CmdletBinding()]
param([string]$OutputDirectory = 'target/distrib/native/windows')

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$nativeRoot = [IO.Path]::GetFullPath((Join-Path $root 'target/distrib/native'))
$outputPath = [IO.Path]::GetFullPath((Join-Path $root $OutputDirectory))
if (-not $outputPath.StartsWith($nativeRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'output must stay below target/distrib/native' }
$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"$').Matches[0].Groups[1].Value
if (-not $version) { throw 'missing workspace version' }
$tools = Get-Content (Join-Path $root 'packaging/native-tools.toml') -Raw
$url = ([regex]::Match($tools, 'url = "([^"]+)"')).Groups[1].Value
$expectedHash = ([regex]::Match($tools, 'sha256 = "([0-9a-f]{64})"')).Groups[1].Value
if (-not $url -or -not $expectedHash) { throw 'missing WiX pin' }
$toolDirectory = Join-Path $root '.scratch/package-windows/tools'
New-Item -ItemType Directory -Force -Path $toolDirectory | Out-Null
$package = Join-Path $toolDirectory 'wix.4.0.5.nupkg'
Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $package
if ((Get-FileHash -Algorithm SHA256 $package).Hash.ToLowerInvariant() -ne $expectedHash) { throw 'WiX checksum mismatch' }
$wixDirectory = Join-Path $toolDirectory 'wix'
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $wixDirectory
Add-Type -AssemblyName System.IO.Compression.FileSystem
[IO.Compression.ZipFile]::ExtractToDirectory($package, $wixDirectory)
$wix = Join-Path $wixDirectory 'tools/net6.0/any/wix.dll'
if (-not (Test-Path $wix)) { throw 'WiX entry point is missing' }
$wixVersion = @(& dotnet $wix --version)
if ($LASTEXITCODE -ne 0 -or $wixVersion.Count -ne 1 -or ([string]$wixVersion[0]).Trim() -ne '4.0.5+b9b2f1b4') { throw 'WiX 4.0.5 cannot run on this runner' }
foreach ($packageName in 'focusforge-cli', 'focusforge-gui') {
  cargo build --locked --release --target x86_64-pc-windows-msvc -p $packageName
  if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
$staging = Join-Path $root '.scratch/package-windows/staging'
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $staging
New-Item -ItemType Directory -Force -Path $staging | Out-Null
$cli = Join-Path $root 'target/x86_64-pc-windows-msvc/release/focusforge-cli.exe'
$gui = Join-Path $root 'target/x86_64-pc-windows-msvc/release/focusforge-gui.exe'
foreach ($path in $cli, $gui, (Join-Path $root 'LICENSE'), (Join-Path $root 'NOTICE'), (Join-Path $root 'assets/build/focusforge.ico')) { if (-not (Test-Path $path)) { throw "missing input: $path" } }
$msi = Join-Path $staging "FocusForge-$version-x86_64.msi"
& dotnet $wix build -arch x64 "-dVersion=$version" "-dCli=$cli" "-dGui=$gui" "-dLicense=$(Join-Path $root 'LICENSE')" "-dNotice=$(Join-Path $root 'NOTICE')" "-dIcon=$(Join-Path $root 'assets/build/focusforge.ico')" -o $msi (Join-Path $root 'packaging/windows/focusforge.wxs')
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$revision = (git -C $root rev-parse HEAD).Trim()
$receipt = [ordered]@{ kind = 'msi'; version = $version; revision = $revision; artifact = (Split-Path $msi -Leaf); sha256 = (Get-FileHash -Algorithm SHA256 $msi).Hash.ToLowerInvariant() } | ConvertTo-Json -Compress
$destination = $outputPath
$replacement = "$destination.next"; $previous = "$destination.previous"
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $replacement, $previous
New-Item -ItemType Directory -Force -Path $replacement | Out-Null
Move-Item $msi (Join-Path $replacement (Split-Path $msi -Leaf))
Set-Content -NoNewline -Encoding utf8 (Join-Path $replacement 'native-receipt.json') $receipt
if (Test-Path $destination) { Move-Item $destination $previous }
try { Move-Item $replacement $destination }
catch { if (Test-Path $previous) { Move-Item $previous $destination }; throw }
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $previous
