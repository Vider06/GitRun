#requires -Version 5.1
[CmdletBinding(DefaultParameterSetName = 'Connect')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Connect')]
    [ValidateNotNullOrEmpty()]
    [string]$Target,

    [Parameter(Mandatory = $true, ParameterSetName = 'Receive')]
    [ValidateNotNullOrEmpty()]
    [string]$Source
)
$ErrorActionPreference = 'Stop'
$MaximumLogBytes = 32 * 1024 * 1024

function Read-ProtocolLine {
    param([System.Net.Sockets.NetworkStream]$Stream)
    $bytes = New-Object 'System.Collections.Generic.List[byte]'
    while ($true) {
        $value = $Stream.ReadByte()
        if ($value -lt 0) { return $null }
        if ($value -eq 10) { break }
        if ($value -ne 13) {
            if ($bytes.Count -ge 512) { throw 'GitRun protocol line is too long.' }
            [void]$bytes.Add([byte]$value)
        }
    }
    return [System.Text.Encoding]::ASCII.GetString($bytes.ToArray())
}
function Write-ProtocolLine {
    param([System.Net.Sockets.NetworkStream]$Stream,[string]$Line)
    $payload = [System.Text.Encoding]::ASCII.GetBytes($Line + [char]10)
    $Stream.Write($payload,0,$payload.Length)
    $Stream.Flush()
}
function Read-ProtocolBytes {
    param([System.Net.Sockets.NetworkStream]$Stream,[int]$Length)
    $buffer = New-Object byte[] $Length
    $offset = 0
    while ($offset -lt $Length) {
        $read = $Stream.Read($buffer,$offset,$Length-$offset)
        if ($read -le 0) { throw 'Connection closed while receiving GitRun logs.' }
        $offset += $read
    }
    return ,$buffer
}
function Get-SavePath {
    param([string]$DefaultName = 'GitRun-logs.txt')
    Add-Type -AssemblyName System.Windows.Forms
    $dialog = New-Object System.Windows.Forms.SaveFileDialog
    $dialog.Title = 'Save GitRun logs'
    $dialog.Filter = 'Text files (*.txt)|*.txt|All files (*.*)|*.*'
    $dialog.FileName = $DefaultName
    $dialog.InitialDirectory = [Environment]::GetFolderPath('MyDocuments')
    try {
        if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { return $dialog.FileName }
        return $null
    }
    finally { $dialog.Dispose() }
}
function Open-LogInNotepad {
    param([string]$Path)
    Start-Process -FilePath 'notepad.exe' -ArgumentList ('"' + $Path + '"')
}
function Save-LogBytesWithDialog {
    param([byte[]]$Bytes)
    $name = 'GitRun-logs-{0}.txt' -f (Get-Date -Format 'yyyyMMdd-HHmmss')
    $destination = Get-SavePath -DefaultName $name
    if ([string]::IsNullOrWhiteSpace($destination)) { return $false }
    [System.IO.File]::WriteAllBytes($destination,$Bytes)
    Open-LogInNotepad -Path $destination
    return $true
}
function Receive-RemoteLogFile {
    param([string]$RemoteSource)
    if ($RemoteSource -notmatch '^(?<user>[A-Za-z0-9._-]+)@(?<host>[A-Za-z0-9.-]+):(?<path>/tmp/GitRun-logs-[a-f0-9]{32}\.txt)$') {
        throw 'Source must be user@host:/tmp/GitRun-logs-<32 hex characters>.txt.'
    }
    $scpCommand = Get-Command 'scp.exe' -ErrorAction SilentlyContinue
    if ($null -eq $scpCommand) { $scpCommand = Get-Command 'scp' -ErrorAction Stop }
    $destination = Get-SavePath -DefaultName ('GitRun-logs-{0}.txt' -f (Get-Date -Format 'yyyyMMdd-HHmmss'))
    if ([string]::IsNullOrWhiteSpace($destination)) { Write-Host 'Save cancelled; no local file was written.'; return }
    $temporary = Join-Path $env:TEMP ('GitRun-logs-download-{0}.txt' -f [Guid]::NewGuid().ToString('N'))
    try {
        & $scpCommand.Source $RemoteSource $temporary
        if ($LASTEXITCODE -ne 0) { throw "scp failed with exit code $LASTEXITCODE." }
        [System.IO.File]::Copy($temporary,$destination,$true)
        Open-LogInNotepad -Path $destination
        Write-Host "Saved logs to $destination and opened Notepad."
    }
    finally { Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue }
}
function Read-ProtocolBytes {
    param([System.Net.Sockets.NetworkStream]$Stream,[int]$Length)
    $buffer = New-Object byte[] $Length
    $offset = 0
    while ($offset -lt $Length) {
        $read = $Stream.Read($buffer,$offset,$Length-$offset)
        if ($read -le 0) { throw 'Connection closed while receiving GitRun logs.' }
        $offset += $read
    }
    return ,$buffer
}
function Invoke-NativeClipboardCopy {
    param([byte[]]$Bytes)
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo.FileName = (Get-Command 'clip.exe' -ErrorAction Stop).Source
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.RedirectStandardInput = $true
    $process.StartInfo.CreateNoWindow = $true
    [void]$process.Start()
    try {
        $process.StandardInput.BaseStream.Write($Bytes,0,$Bytes.Length)
        $process.StandardInput.Close()
        if (-not $process.WaitForExit(2000)) {
            try { $process.Kill() } catch {}
            return $false
        }
        return ($process.ExitCode -eq 0)
    }
    finally { $process.Dispose() }
}
function Handle-LogExportClient {
    param([System.Net.Sockets.TcpClient]$Client,[string]$ExpectedToken)
    $stream = $Client.GetStream()
    $stream.ReadTimeout = 5000
    $stream.WriteTimeout = 5000
    try {
        $hello = Read-ProtocolLine -Stream $stream
        if ($hello -cne ('GITRUN_LOGS_V1 ' + $ExpectedToken)) { Write-ProtocolLine $stream 'DENIED'; return }
        Write-ProtocolLine $stream 'READY'
        $header = Read-ProtocolLine -Stream $stream
        if ($header -notmatch '^LOGS ([0-9]{1,8})$') { Write-ProtocolLine $stream 'ERROR invalid log payload header'; return }
        $length = [int]$Matches[1]
        if ($length -gt $MaximumLogBytes) { Write-ProtocolLine $stream 'ERROR log payload exceeds limit'; return }
        $bytes = Read-ProtocolBytes -Stream $stream -Length $length
        if (Invoke-NativeClipboardCopy -Bytes $bytes) {
            Write-ProtocolLine $stream 'COPIED'
            return
        }
        Write-ProtocolLine $stream 'FALLBACK'
        $stream.ReadTimeout = 600000
        $fallbackHeader = Read-ProtocolLine -Stream $stream
        if ($fallbackHeader -notmatch '^LOGS ([0-9]{1,8})$') { Write-ProtocolLine $stream 'ERROR invalid fallback payload header'; return }
        $fallbackLength = [int]$Matches[1]
        if ($fallbackLength -gt $MaximumLogBytes) { Write-ProtocolLine $stream 'ERROR fallback exceeds limit'; return }
        $fallbackBytes = Read-ProtocolBytes -Stream $stream -Length $fallbackLength
        if (Save-LogBytesWithDialog -Bytes $fallbackBytes) { Write-ProtocolLine $stream 'SAVED' }
        else { Write-ProtocolLine $stream 'CANCELLED' }
    }
    catch {
        try { Write-ProtocolLine $stream 'ERROR receiver failed' } catch {}
        Write-Warning $_.Exception.Message
    }
    finally { $stream.Dispose(); $Client.Close() }
}

if ($PSCmdlet.ParameterSetName -eq 'Receive') {
    Receive-RemoteLogFile -RemoteSource $Source
    return
}
if ($Target -notmatch '^[A-Za-z0-9._@:-]+$' -or $Target.StartsWith('-')) {
    throw 'Target must be a simple SSH target such as user@MainNode or 100.x.y.z.'
}
$ssh = Get-Command 'ssh.exe' -ErrorAction SilentlyContinue
if ($null -eq $ssh) { $ssh = Get-Command 'ssh' -ErrorAction Stop }
$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback,0)
$listener.Start()
$localPort = ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
$remotePort = Get-Random -Minimum 41000 -Maximum 58000
$token = [Guid]::NewGuid().ToString('N')
$tokenFilePath = '/tmp/GitRun-logs-token-{0}.txt' -f $token
$forward = '127.0.0.1:{0}:127.0.0.1:{1}' -f $remotePort,$localPort

# Send the random token through SSH stdin into a user-owned 0600 file.
# Keeping the token out of command-line arguments prevents other host users
# from reading it via process listings while the console is running.
$tokenStartInfo = New-Object System.Diagnostics.ProcessStartInfo
$tokenStartInfo.FileName = $ssh.Source
$tokenStartInfo.UseShellExecute = $false
$tokenStartInfo.RedirectStandardInput = $true
$tokenStartInfo.CreateNoWindow = $false
$tokenStartInfo.Arguments = @('-T',$Target,('umask 077; cat > {0} && chmod 600 {0}' -f $tokenFilePath)) -join ' '
$tokenProcess = $null
try {
    $tokenProcess = [System.Diagnostics.Process]::Start($tokenStartInfo)
    $tokenProcess.StandardInput.WriteLine($token)
    $tokenProcess.StandardInput.Close()
    $tokenProcess.WaitForExit()
    if ($tokenProcess.ExitCode -ne 0) {
        throw "Could not create the private GitRun log-export token file (ssh exit $($tokenProcess.ExitCode))."
    }
}
finally {
    if ($null -ne $tokenProcess) { $tokenProcess.Dispose() }
}

$remoteCommand = "trap 'rm -f $tokenFilePath' EXIT; GITRUN_LOG_EXPORT_PORT=$remotePort GITRUN_LOG_EXPORT_TOKEN_FILE=$tokenFilePath gitrun manage"
$startInfo = New-Object System.Diagnostics.ProcessStartInfo
$startInfo.FileName = $ssh.Source
$startInfo.UseShellExecute = $false
$startInfo.CreateNoWindow = $false
$startInfo.Arguments = @('-o','ExitOnForwardFailure=no','-R',$forward,'-tt',$Target,$remoteCommand) -join ' '
$sshProcess = $null
try {
    $sshProcess = [System.Diagnostics.Process]::Start($startInfo)
    while (-not $sshProcess.HasExited) {
        if ($listener.Pending()) {
            $client = $listener.AcceptTcpClient()
            Handle-LogExportClient -Client $client -ExpectedToken $token
        } else { Start-Sleep -Milliseconds 40 }
        $sshProcess.Refresh()
    }
    $sshProcess.WaitForExit()
    if ($sshProcess.ExitCode -ne 0) { Write-Warning "SSH exited with code $($sshProcess.ExitCode). Confirm that remote port forwarding is allowed." }
    return
}
finally {
    if ($null -ne $sshProcess) {
        try { if (-not $sshProcess.HasExited) { $sshProcess.Kill(); $sshProcess.WaitForExit() } } catch {}
        $sshProcess.Dispose()
    }
    $listener.Stop()
}
