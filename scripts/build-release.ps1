$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Push-Location $Root
try {
    cargo build --release -p gitrun-cli
    New-Item -ItemType Directory -Force -Path (Join-Path $Root "dist") | Out-Null
    Write-Host "GitRun release build complete"
} finally { Pop-Location }
