# Builds hilo-windows.zip: hilo-app.exe (the app) and hilo.exe (the CLI) side by side.
#
#   pwsh app/bundle.ps1                   # or: powershell -File app\bundle.ps1
#
# Unzip it anywhere and open hilo-app.exe: it runs the hilo.exe next to it. Not signed, no installer.
#
# The app builds from outside the repo's cargo config: that config swaps crates.io for ../vendor
# (the CLI's offline build), and gpui's dependencies are not vendored.
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path "$PSScriptRoot\..").Path
$App = Join-Path $Root 'app'
$Out = Join-Path $App 'dist'
$Zip = Join-Path $Out 'hilo-windows.zip'

Push-Location $Root
try {
    cargo build --release --offline --locked
    if ($LASTEXITCODE -ne 0) { throw "building the hilo CLI failed (cargo exited with $LASTEXITCODE)" }
} finally { Pop-Location }

# from the drive's root, like bundle.sh's `cd /`: cargo reads the config of the directory it runs in
Push-Location ([IO.Path]::GetPathRoot($Root))
try {
    cargo build --release --locked --manifest-path (Join-Path $App 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw "building hilo-app failed (cargo exited with $LASTEXITCODE)" }
} finally { Pop-Location }

$Exes = @(
    (Join-Path (Join-Path (Join-Path $App 'target') 'release') 'hilo-app.exe'),
    (Join-Path (Join-Path (Join-Path $Root 'target') 'release') 'hilo.exe')
)
foreach ($Exe in $Exes) {
    if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) { throw "missing ${Exe}: the build did not leave it there" }
}

New-Item -ItemType Directory -Force -Path $Out | Out-Null
# a stable name, like hilo-macos.dmg
if (Test-Path -LiteralPath $Zip) { Remove-Item -LiteralPath $Zip -Force }
Compress-Archive -LiteralPath $Exes -DestinationPath $Zip
Write-Output $Zip
