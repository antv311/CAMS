param(
    [string]$Bind = "0.0.0.0:6667",
    [string]$HttpBind = "0.0.0.0:6680",
    [string]$DbPath = "$env:LOCALAPPDATA\aircd\aircd.sqlite3",
    [switch]$Background
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
$DbDir = Split-Path -Parent $DbPath
if ($DbDir -and !(Test-Path $DbDir)) {
    New-Item -ItemType Directory -Path $DbDir -Force | Out-Null
}

$ReleaseExe = Join-Path $RepoRoot "target\release\aircd.exe"
$DebugExe = Join-Path $RepoRoot "target\debug\aircd.exe"
if (Test-Path $ReleaseExe) {
    $Exe = $ReleaseExe
} elseif (Test-Path $DebugExe) {
    $Exe = $DebugExe
} else {
    Write-Host "Building aircd..."
    & cargo build --manifest-path (Join-Path $RepoRoot "Cargo.toml")
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $Exe = $DebugExe
}

$ArgList = @("--bind", $Bind, "--http-bind", $HttpBind, "--db", $DbPath)
if ($Background) {
    $LogOut = Join-Path $DbDir "aircd.stdout.log"
    $LogErr = Join-Path $DbDir "aircd.stderr.log"
    $Proc = Start-Process -FilePath $Exe -ArgumentList $ArgList -RedirectStandardOutput $LogOut -RedirectStandardError $LogErr -PassThru -WindowStyle Hidden
    Write-Host "Started aircd (PID $($Proc.Id))"
    Write-Host "Welcome/API: http://127.0.0.1:$(($HttpBind -split ':')[-1])/"
    Write-Host "IRC TCP:     127.0.0.1:$(($Bind -split ':')[-1])"
    Write-Host "Logs:        $LogOut"
} else {
    & $Exe @ArgList
}
