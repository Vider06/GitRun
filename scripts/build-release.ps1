$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Push-Location $Root
try {
    $Target = if ($args.Count -ge 1) { $args[0] } else { rustc -vV | Select-String 'host:' | ForEach-Object { ($_ -split '\s+')[1] } }
    $Version = if ($args.Count -ge 2) { $args[1] } else { git describe --tags --always --dirty }
    cargo build --release -p gitrun-cli --target $Target
    $binary = if ($Target -like '*windows*') { 'gitrun-rs.exe' } else { 'gitrun-rs' }
    $out = Join-Path $Root 'distelease'
    New-Item -ItemType Directory -Force -Path $out | Out-Null
    Copy-Item (Join-Path $Root "target$Targetelease$binary") $out
    Copy-Item (Join-Path $Root 'LICENSE'), (Join-Path $Root 'README.md'), (Join-Path $Root 'configconfig.example.env') $out
    Write-Host "GitRun release build complete: $Target"
} finally { Pop-Location }
