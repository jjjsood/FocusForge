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
$msiVersion = ($version -split '-')[0]
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
$cliPdb = Join-Path (Split-Path $cli) (@(& python $root/tools/package-symbols.py windows-pdb $cli))
$guiPdb = Join-Path (Split-Path $gui) (@(& python $root/tools/package-symbols.py windows-pdb $gui))
foreach ($path in $cliPdb, $guiPdb) { if (-not (Test-Path $path)) { throw "missing PDB: $path" } }
$dumpSyms = @(& python $root/tools/package-symbols.py install dump_syms x86_64-pc-windows-msvc (Join-Path $root '.scratch/package-windows/symbol-tools'))
if ($LASTEXITCODE -ne 0 -or $dumpSyms.Count -ne 1) { throw 'pinned dump_syms cannot run on this runner' }
& python $root/tools/package-symbols.py verify-windows "--identity=focusforge-cli=$cli,$cliPdb" "--identity=focusforge-gui=$gui,$guiPdb"
if ($LASTEXITCODE -ne 0) { throw 'PE/PDB native GUID+age mismatch' }
foreach ($pair in @(@($cli, $cliPdb), @($gui, $guiPdb))) {
  $peModule = @(& $dumpSyms[0] --inlines $pair[0] | Select-String '^MODULE ')
  if ($LASTEXITCODE -ne 0) { throw "cannot read PE debug identity: $($pair[0])" }
  $pdbModule = @(& $dumpSyms[0] --inlines $pair[1] | Select-String '^MODULE ')
  if ($LASTEXITCODE -ne 0 -or $peModule.Count -ne 1 -or $pdbModule.Count -ne 1 -or $peModule[0].Line -ne $pdbModule[0].Line) { throw "PE/PDB GUID+age mismatch: $($pair[0])" }
}
foreach ($path in $cli, $gui, (Join-Path $root 'LICENSE'), (Join-Path $root 'NOTICE'), (Join-Path $root 'assets/build/focusforge.ico')) { if (-not (Test-Path $path)) { throw "missing input: $path" } }
$native = Join-Path $staging 'native'; New-Item -ItemType Directory -Force -Path $native | Out-Null
$resources = Join-Path $staging 'resources'; & python $root/tools/package-cli-assets.py stage $root $resources
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Copy-Item $cliPdb, $guiPdb -Destination $native -Force
$symbolOutput = Join-Path $staging 'symbols/x86_64-pc-windows-msvc'
& python $root/tools/package-symbols.py build --platform x86_64-pc-windows-msvc --version $version --dump-syms $dumpSyms[0] --output $symbolOutput --output-root $staging --symbols-per-native 1 "--native=focusforge-cli=$(Join-Path $native (Split-Path $cliPdb -Leaf))" "--native=focusforge-gui=$(Join-Path $native (Split-Path $guiPdb -Leaf))" "--binary=focusforge-cli=$cli" "--binary=focusforge-gui=$gui"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$msi = Join-Path $staging "FocusForge-$version-x86_64.msi"
& dotnet $wix build -arch x64 -d "Version=$msiVersion" -d "Cli=$cli" -d "Gui=$gui" -d "License=$(Join-Path $root 'LICENSE')" -d "Notice=$(Join-Path $root 'NOTICE')" -d "Icon=$(Join-Path $root 'assets/build/focusforge.ico')" -d "CliAssets=$(Join-Path $resources 'share')" -o $msi (Join-Path $root 'packaging/windows/focusforge.wxs')
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$revision = (git -C $root rev-parse HEAD).Trim()
$receipt = [ordered]@{ kind = 'msi'; version = $version; revision = $revision; artifact = (Split-Path $msi -Leaf); sha256 = (Get-FileHash -Algorithm SHA256 $msi).Hash.ToLowerInvariant() } | ConvertTo-Json -Compress
$destination = $outputPath
$replacement = "$destination.next"; $previous = "$destination.previous"
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $replacement, $previous
New-Item -ItemType Directory -Force -Path $replacement | Out-Null
Move-Item $msi (Join-Path $replacement (Split-Path $msi -Leaf))
Set-Content -NoNewline -Encoding utf8 (Join-Path $replacement 'native-receipt.json') $receipt
New-Item -ItemType Directory -Force -Path (Join-Path $replacement 'symbols') | Out-Null
Move-Item $symbolOutput (Join-Path $replacement 'symbols/x86_64-pc-windows-msvc')
if (Test-Path $destination) { Move-Item $destination $previous }
try { Move-Item $replacement $destination }
catch { if (Test-Path $previous) { Move-Item $previous $destination }; throw }
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $previous
