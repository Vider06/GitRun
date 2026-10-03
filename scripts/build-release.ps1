$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Push-Location $Root
try {
    $Target = if ($args.Count -ge 1) {
        [string]$args[0]
    } else {
        (rustc -vV | Select-String 'host:' | ForEach-Object { ($_ -split '\s+')[1] })
    }

    $Version = if ($args.Count -ge 2) {
        [string]$args[1]
    } else {
        git describe --tags --always --dirty
    }

    if ($Version.StartsWith('v')) {
        $Version = $Version.Substring(1)
    }

    if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$') {
        throw "Invalid GitRun release version: $Version"
    }

    $HostTarget = (rustc -vV | Select-String 'host:' | ForEach-Object { ($_ -split '\s+')[1] })
    if ($Target -ne $HostTarget) {
        throw "Tauri local release builds must target the current Rust host ($HostTarget); got $Target"
    }

    cargo build --locked --release --target $Target -p gitrun-cli --bin gitrun

    $gitrunBinary = if ($Target -like '*windows*') { 'gitrun.exe' } else { 'gitrun' }
    $binaryRoot = Join-Path $Root "target\$Target\release"
    $gitrunPath = Join-Path $binaryRoot $gitrunBinary
    if (-not (Test-Path $gitrunPath -PathType Leaf)) {
        throw "Unified GitRun executable was not produced: $gitrunPath"
    }

    $release = Join-Path $Root 'dist\release'
    $package = Join-Path $release 'package'
    New-Item -ItemType Directory -Force -Path $package | Out-Null

    Get-ChildItem $release -Filter 'GitRun-*' -File -ErrorAction SilentlyContinue | Remove-Item -Force
    Get-ChildItem $package -Force -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force

    Copy-Item $gitrunPath $package
    Copy-Item (Join-Path $Root 'LICENSE'), (Join-Path $Root 'README.md'), (Join-Path $Root 'config\config.example.env'), (Join-Path $Root 'version.txt') $package

    $archive = Join-Path $release "GitRun-$Version-$Target.zip"
    Compress-Archive -Path (Join-Path $package '*') -DestinationPath $archive -Force
    $hash = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower()
    [IO.File]::WriteAllText(
        "$archive.sha256",
        "$hash  $(Split-Path $archive -Leaf)$([Environment]::NewLine)",
        [Text.UTF8Encoding]::new($false)
    )

    $manifest = @{
        name = 'GitRun'
        version = $Version
        git_commit = (git rev-parse HEAD)
        artifacts = @(@{ target = $Target; file = (Split-Path $archive -Leaf); sha256 = $hash })
    } | ConvertTo-Json -Depth 4
    [IO.File]::WriteAllText(
        (Join-Path $release 'release-manifest.json'),
        $manifest + [Environment]::NewLine,
        [Text.UTF8Encoding]::new($false)
    )

    Write-Host "GitRun release build complete: $Target -> $archive"
    Write-Host "Included: $gitrunBinary"
    } finally {
    Pop-Location
}
