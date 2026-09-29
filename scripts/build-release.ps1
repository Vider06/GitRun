$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Push-Location $Root
try {
    $Target = if ($args.Count -ge 1) {
        $args[0]
    } else {
        (rustc -vV | Select-String 'host:' | ForEach-Object { ($_ -split '\s+')[1] })
    }

    $Version = if ($args.Count -ge 2) {
        $args[1]
    } else {
        git describe --tags --always --dirty
    }

    cargo build --locked --release -p gitrun-cli --bin gitrun

    if (-not (Get-Command npm -ErrorAction SilentlyContinue)) { throw "npm is required to build the Tauri dashboard" }
    Push-Location "crates\gitrun-dashboard-tauri"
    try {
        npm install --ignore-scripts --no-audit --no-fund
        npm run tauri build -- --ci --no-bundle
    } finally { Pop-Location }

    $cliBinary = if ($Target -like '*windows*') { 'gitrun.exe' } else { 'gitrun' }
    $dashboardBinary = if ($Target -like '*windows*') { 'gitrun-dashboard-tauri.exe' } else { 'gitrun-dashboard-tauri' }

    $release = Join-Path $Root 'dist\release'
    $package = Join-Path $release 'package'
    New-Item -ItemType Directory -Force -Path $package | Out-Null

    Get-ChildItem $release -Filter 'GitRun-*' -File -ErrorAction SilentlyContinue | Remove-Item -Force
    Get-ChildItem $package -Force -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force

    Copy-Item (Join-Path $Root "target\release\$cliBinary") $package
    Copy-Item (Join-Path $Root "target\release\$dashboardBinary") $package
    Copy-Item (Join-Path $Root 'LICENSE'), (Join-Path $Root 'README.md'), (Join-Path $Root 'config\config.example.env') $package

    $archive = Join-Path $release "GitRun-$Version-$Target.zip"
    Compress-Archive -Path (Join-Path $package '*') -DestinationPath $archive -Force
    $hash = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower()
    "$hash  $(Split-Path $archive -Leaf)" | Set-Content "$archive.sha256"

    Write-Host "GitRun release build complete: $Target -> $archive"
    Write-Host "Included: $cliBinary, $dashboardBinary"
} finally {
    Pop-Location
}
