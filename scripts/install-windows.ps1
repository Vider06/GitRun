$ErrorActionPreference = "Stop"

$RepoUrl = "https://github.com/Vider06/GitRun.git"
$InstallDir = if ($env:GITRUN_INSTALL_DIR) { $env:GITRUN_INSTALL_DIR } else { Join-Path $HOME ".gitrun" }
$ConfigDir = Join-Path $InstallDir "config"
$EnvFile = Join-Path $ConfigDir "gitrun.env"
$ComposeFile = Join-Path $InstallDir "docker-compose.yml"

function Assert-LastExitCode {
  param([string]$Operation)

  if ($LASTEXITCODE -ne 0) {
    throw "$Operation failed with exit code $LASTEXITCODE."
  }
}

if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
  throw "winget is required for automatic dependency installation."
}

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
  winget install --id Docker.DockerDesktop --exact --accept-package-agreements --accept-source-agreements
  Assert-LastExitCode "Docker Desktop installation"
}

if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
  winget install --id Git.Git --exact --accept-package-agreements --accept-source-agreements
  Assert-LastExitCode "Git installation"
}

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
  throw "Docker is still unavailable after installation. Restart PowerShell and run the installer again."
}

if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
  throw "Git is still unavailable after installation. Restart PowerShell and run the installer again."
}

$ready = $false
for ($i = 0; $i -lt 60; $i++) {
  try {
    docker info | Out-Null
    if ($LASTEXITCODE -eq 0) {
      $ready = $true
      break
    }
  } catch {
    # Docker CLI can return a native non-zero exit code without throwing.
  }
  Start-Sleep -Seconds 2
}

if (-not $ready) {
  $exe = Join-Path $env:ProgramFiles "Docker\Docker\Docker Desktop.exe"
  if (Test-Path $exe) {
    Start-Process $exe
  }
  for ($i = 0; $i -lt 60; $i++) {
    try {
      docker info | Out-Null
      if ($LASTEXITCODE -eq 0) {
        $ready = $true
        break
      }
    } catch {
      # Keep waiting for Docker Desktop to finish starting.
    }
    Start-Sleep -Seconds 2
  }
}

if (-not $ready) {
  throw "Docker Desktop is not running."
}

docker compose version | Out-Null
Assert-LastExitCode "Docker Compose check"

if (Test-Path (Join-Path $InstallDir ".git")) {
  git -C $InstallDir remote get-url origin | Out-Null
  Assert-LastExitCode "Git repository check"

  $origin = git -C $InstallDir remote get-url origin
  Assert-LastExitCode "Git repository origin lookup"
  if ($origin.TrimEnd("/") -ne $RepoUrl.TrimEnd("/")) {
    throw "Existing GitRun checkout has an unexpected origin: $origin"
  }

  git -C $InstallDir pull --ff-only
  Assert-LastExitCode "GitRun source update"
} else {
  if (Test-Path $InstallDir) {
    $entries = @(Get-ChildItem -LiteralPath $InstallDir -Force)
    if ($entries.Count -gt 0) {
      throw "Install directory exists and is not a GitRun checkout: $InstallDir"
    }
  } else {
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
  }

  git clone $RepoUrl $InstallDir
  Assert-LastExitCode "GitRun source checkout"
}

if (-not (Test-Path $ComposeFile)) {
  throw "GitRun checkout is incomplete; docker-compose.yml was not found at $ComposeFile."
}

New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null

$currentUser = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
icacls $ConfigDir /inheritance:r | Out-Null
Assert-LastExitCode "Config directory ACL hardening"
icacls $ConfigDir /grant:r "${currentUser}:(OI)(CI)(F)" "*S-1-5-18:(OI)(CI)(F)" "*S-1-5-32-544:(OI)(CI)(F)" | Out-Null
Assert-LastExitCode "Config directory ACL assignment"

if (-not (Test-Path $EnvFile)) {
  Copy-Item (Join-Path $InstallDir "config\config.example.env") $EnvFile
}

$token = Read-Host "GitHub token" -AsSecureString
$repos = Read-Host "Repositories (comma separated)"

if ([string]::IsNullOrWhiteSpace($repos)) {
  throw "At least one repository is required."
}

$tokenPtr = [IntPtr]::Zero
try {
  $tokenPtr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($token)
  $plainToken = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($tokenPtr)
  if ([string]::IsNullOrWhiteSpace($plainToken)) {
    throw "GitHub token is required."
  }

  $repoList = @($repos.Split(",") |
    ForEach-Object { $_.Trim() } |
    Where-Object { $_ -ne "" })

  if ($repoList.Count -eq 0) {
    throw "At least one repository is required."
  }

  foreach ($repo in $repoList) {
    if ($repo -notmatch "^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$") {
      throw "Invalid repository: $repo"
    }

    $parts = $repo.Split("/", 2)
    try {
      $headers = @{
        Accept = "application/vnd.github+json"
        Authorization = "Bearer $plainToken"
        "X-GitHub-Api-Version" = "2026-03-10"
        "User-Agent" = "GitRun-Windows-Installer"
      }
      Invoke-RestMethod -Uri "https://api.github.com/repos/$($parts[0])/$($parts[1])" -Headers $headers -Method Get -TimeoutSec 20 | Out-Null
    } catch {
      throw "GitHub access check failed for $repo. Verify the token and repository permissions."
    }
  }

  $lines = Get-Content -LiteralPath $EnvFile
  $updatedLines = foreach ($line in $lines) {
    if ($line -like "GITHUB_TOKEN=*") {
      "GITHUB_TOKEN=$plainToken"
    } elseif ($line -like "GITRUN_REPOSITORIES=*") {
      "GITRUN_REPOSITORIES=$($repoList -join ",")"
    } else {
      $line
    }
  }

  $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
  $tempEnv = Join-Path $ConfigDir (".gitrun.env.{0}.tmp" -f ([guid]::NewGuid().ToString("N")))
  try {
    [System.IO.File]::WriteAllLines($tempEnv, [string[]]$updatedLines, $utf8NoBom)
    Move-Item -LiteralPath $tempEnv -Destination $EnvFile -Force
  } finally {
    if (Test-Path -LiteralPath $tempEnv) {
      Remove-Item -LiteralPath $tempEnv -Force
    }
  }
} finally {
  if ($tokenPtr -ne [IntPtr]::Zero) {
    [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($tokenPtr)
  }
  $plainToken = $null
  $token = $null
}

icacls $EnvFile /inheritance:r | Out-Null
Assert-LastExitCode "Credential file ACL hardening"
icacls $EnvFile /grant:r "${currentUser}:F" "*S-1-5-18:F" "*S-1-5-32-544:F" | Out-Null
Assert-LastExitCode "Credential file ACL assignment"

$state = Join-Path $InstallDir "state"
$logs = Join-Path $InstallDir "logs"
New-Item -ItemType Directory -Force -Path $state, $logs | Out-Null
$env:GITRUN_CONFIG_FILE = $EnvFile
$env:GITRUN_STATE_DIR = $state
$env:GITRUN_LOG_DIR = $logs
$env:GITRUN_DOCKER_SOCKET = "/var/run/docker.sock"

docker compose --env-file $EnvFile -f $ComposeFile config -q
Assert-LastExitCode "GitRun Compose configuration validation"

docker compose --env-file $EnvFile -f $ComposeFile up -d --build
Assert-LastExitCode "GitRun container startup"

$healthy = $false
for ($i = 0; $i -lt 30; $i++) {
  $status = docker inspect --format '{{.State.Health.Status}}' gitrun-manager 2>$null
  if ($LASTEXITCODE -eq 0 -and $status -eq "healthy") {
    $healthy = $true
    break
  }
  Start-Sleep -Seconds 2
}

if (-not $healthy) {
  docker compose --env-file $EnvFile -f $ComposeFile ps
  docker compose --env-file $EnvFile -f $ComposeFile logs --tail 50 gitrun-manager
  throw "GitRun manager did not become healthy."
}

Write-Host "GitRun installed and started."
