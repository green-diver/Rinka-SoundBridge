param([switch]$RegisterAsio)
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot

$localCargo = Join-Path $PSScriptRoot '.tools\cargo\bin\cargo.exe'
if (Test-Path -LiteralPath $localCargo) {
    $env:CARGO_HOME = Join-Path $PSScriptRoot '.tools\cargo'
    $env:RUSTUP_HOME = Join-Path $PSScriptRoot '.tools\rustup'
    $cargoCommand = $localCargo
} elseif (Get-Command cargo -ErrorAction SilentlyContinue) {
    $cargoCommand = (Get-Command cargo).Source
} else {
    throw 'Install stable Rust x86_64-pc-windows-msvc and Visual Studio C++ Build Tools first.'
}

& $cargoCommand build --release --locked
if ($LASTEXITCODE -ne 0) { throw 'Cargo build failed.' }

$packageDir = Join-Path $PSScriptRoot 'dist'
New-Item -ItemType Directory -Path $packageDir -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $packageDir 'Modules') -Force | Out-Null
Copy-Item -LiteralPath 'target\release\RinkaSoundBridge.exe' -Destination $packageDir -Force
Copy-Item -LiteralPath 'target\release\lan_echo.exe' -Destination $packageDir -Force
Copy-Item -LiteralPath 'target\release\rinka_soundbridge_core.dll' -Destination (Join-Path $packageDir 'Modules') -Force
Copy-Item -LiteralPath 'icon.ico','LAN_SETUP.md' -Destination $packageDir -Force
Set-Content -LiteralPath (Join-Path $packageDir 'Start LAN Client.cmd') -Encoding ascii -Value @(
    '@echo off', 'start "" "%~dp0RinkaSoundBridge.exe" --lan-client'
)
Set-Content -LiteralPath (Join-Path $packageDir 'Start LAN Server.cmd') -Encoding ascii -Value @(
    '@echo off', 'start "" "%~dp0RinkaSoundBridge.exe" --lan-server'
)
if ($RegisterAsio) {
    & (Join-Path $packageDir 'RinkaSoundBridge.exe') --register-asio
    if ($LASTEXITCODE -ne 0) { throw 'ASIO registration failed.' }
}
$packageItems = @('RinkaSoundBridge.exe','lan_echo.exe','icon.ico','LAN_SETUP.md','Modules','Start LAN Client.cmd','Start LAN Server.cmd') |
    ForEach-Object { Join-Path $packageDir $_ }
Compress-Archive -Path $packageItems -DestinationPath (Join-Path $PSScriptRoot 'RinkaSoundBridge-LAN.zip') -Force
Write-Output "Built $packageDir and RinkaSoundBridge-LAN.zip. Use the Client or Server launcher on each PC."
