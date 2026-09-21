# GitRun Windows installer
$ErrorActionPreference = "Stop"
$RepoUrl = if ($env:GITRUN_REPO_URL) {$env:GITRUN_REPO_URL} else {"https://github.com/Vider06/GitRun.git"}
$InstallDir = if ($env:GITRUN_INSTALL_DIR) {$env:GITRUN_INSTALL_DIR} else {Join-Path $HOME ".gitrun"}
$ConfigDir = Join-Path $InstallDir "config"; $EnvFile = Join-Path $ConfigDir "gitrun.env"

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
  if (Get-Command winget -ErrorAction SilentlyContinue) {
    winget install --id Docker.DockerDesktop --exact --accept-package-agreements --accept-source-agreements
  } else { throw "winget is required for automatic Docker Desktop installation." }
}
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
  if (Get-Command winget -ErrorAction SilentlyContinue) {
    winget install --id Git.Git --exact --accept-package-agreements --accept-source-agreements
  } else { throw "Git is required." }
}

$ready=$false
for($i=0;$i -lt 60;$i++){try{docker info|Out-Null;$ready=$true;break}catch{Start-Sleep 2}}
if(-not $ready){
  $exe=Join-Path $env:ProgramFiles "Docker\Docker\Docker Desktop.exe"
  if(Test-Path $exe){Start-Process $exe}
  for($i=0;$i -lt 60;$i++){try{docker info|Out-Null;$ready=$true;break}catch{Start-Sleep 2}}
}
if(-not $ready){throw "Docker Desktop is not running."}
docker compose version | Out-Null

if(Test-Path (Join-Path $InstallDir ".git")){git -C $InstallDir pull --ff-only}else{git clone $RepoUrl $InstallDir}
New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
if(-not(Test-Path $EnvFile)){Copy-Item (Join-Path $InstallDir "config\config.example.env") $EnvFile}

$token=Read-Host "GitHub token"
$repos=Read-Host "Repositories (comma separated)"
$lines=Get-Content $EnvFile|ForEach-Object{
  if($_ -like "GITHUB_TOKEN=*"){"GITHUB_TOKEN=$token"}
  elseif($_ -like "GITRUN_REPOSITORIES=*"){"GITRUN_REPOSITORIES=$repos"}
  else{$_}
}
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllLines($EnvFile, [string[]]$lines, $utf8NoBom)

$state=Join-Path $InstallDir "state"
$logs=Join-Path $InstallDir "logs"
New-Item -ItemType Directory -Force -Path $state,$logs|Out-Null
$env:GITRUN_CONFIG_FILE=$EnvFile
$env:GITRUN_STATE_DIR=$state
$env:GITRUN_LOG_DIR=$logs
$env:GITRUN_DOCKER_SOCKET="/var/run/docker.sock"

docker compose --env-file $EnvFile -f (Join-Path $InstallDir "docker-compose.yml") up -d --build
Write-Host "GitRun installed and started."
