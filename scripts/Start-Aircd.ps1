param(
    [string]$ConfigPath = "",
    [string]$Bind = "127.0.0.1:6667",
    [string]$HttpBind = "127.0.0.1:6680",
    [string]$DbPath = "",
    [switch]$OpenRegistration,
    [string]$RegisterKey = "",
    [string]$TlsCert = "",
    [string]$TlsKey = "",
    [string]$TlsBind = "127.0.0.1:6697",
    [switch]$WindowsEventLog,
    [string]$WindowsEventLogSource = "",
    [string]$SyslogTarget = "",
    [string]$SyslogFormat = "",
    [string]$SyslogFacility = "",
    [switch]$NoBackups,
    [string]$BackupDir = "",
    [string]$BackupIntervalSecs = "",
    [string]$BackupRetention = "",
    [switch]$BackupOnStartup,
    [string]$ExecutablePath = "",
    [switch]$Background,
    # Do nothing when a hub is already listening (used by the sign-in shortcut).
    [switch]$IfNotRunning
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
$ConfigModule = Join-Path $PSScriptRoot "AircdConfig.psm1"
if (Test-Path -LiteralPath $ConfigModule -PathType Leaf) {
    Import-Module $ConfigModule -Force
    $Overrides = @{}
    foreach ($Key in @("Bind", "HttpBind", "DbPath", "OpenRegistration", "RegisterKey", "TlsCert", "TlsKey", "TlsBind", "WindowsEventLog", "WindowsEventLogSource", "SyslogTarget", "SyslogFormat", "SyslogFacility", "BackupDir", "BackupIntervalSecs", "BackupRetention", "BackupOnStartup", "ExecutablePath")) {
        if ($PSBoundParameters.ContainsKey($Key)) {
            $Overrides[$Key] = $PSBoundParameters[$Key]
        }
    }
    if ($NoBackups) { $Overrides["BackupEnabled"] = $false }
    $ResolvedCfg = Resolve-AircdWindowsConfig -RepoRoot $RepoRoot -ConfigPath $ConfigPath -Overrides $Overrides
    $Bind = $ResolvedCfg.Bind
    $HttpBind = $ResolvedCfg.HttpBind
    $DbPath = $ResolvedCfg.DbPath
    $OpenRegistration = [bool]$ResolvedCfg.OpenRegistration
    $RegisterKey = $ResolvedCfg.RegisterKey
    $TlsCert = $ResolvedCfg.TlsCert
    $TlsKey = $ResolvedCfg.TlsKey
    $TlsBind = $ResolvedCfg.TlsBind
    $WindowsEventLog = [bool]$ResolvedCfg.WindowsEventLog
    $WindowsEventLogSource = $ResolvedCfg.WindowsEventLogSource
    $SyslogTarget = $ResolvedCfg.SyslogTarget
    $SyslogFormat = $ResolvedCfg.SyslogFormat
    $SyslogFacility = $ResolvedCfg.SyslogFacility
    $BackupEnabled = [bool]$ResolvedCfg.BackupEnabled
    $BackupDir = $ResolvedCfg.BackupDir
    $BackupIntervalSecs = $ResolvedCfg.BackupIntervalSecs
    $BackupRetention = $ResolvedCfg.BackupRetention
    $BackupOnStartup = [bool]$ResolvedCfg.BackupOnStartup
    $ExecutablePath = $ResolvedCfg.ExecutablePath
} else {
    $BackupEnabled = -not $NoBackups
    if (-not $PSBoundParameters.ContainsKey("Bind") -and $env:AIRCD_BIND) { $Bind = $env:AIRCD_BIND }
    if (-not $PSBoundParameters.ContainsKey("HttpBind") -and $env:AIRCD_HTTP_BIND) { $HttpBind = $env:AIRCD_HTTP_BIND }
    if (-not $PSBoundParameters.ContainsKey("DbPath")) {
        if ($env:AIRCD_DB) {
            $DbPath = $env:AIRCD_DB
        } else {
            $LocalBase = if ($env:LOCALAPPDATA) { $env:LOCALAPPDATA } else { [Environment]::GetFolderPath("LocalApplicationData") }
            $DbPath = Join-Path $LocalBase "aircd\aircd.sqlite3"
        }
    }
}

if ($IfNotRunning) {
    $HttpPort = [int]($HttpBind -split ':')[-1]
    if (Get-NetTCPConnection -LocalPort $HttpPort -State Listen -ErrorAction SilentlyContinue) {
        Write-Host "aircd is already listening on port $HttpPort"
        exit 0
    }
}

$DbDir = if ($DbPath -eq ":memory:") { "" } else { Split-Path -Parent $DbPath }
if ($DbDir -and !(Test-Path $DbDir)) {
    New-Item -ItemType Directory -Path $DbDir -Force | Out-Null
}

$HasTlsCert = -not [string]::IsNullOrWhiteSpace($TlsCert)
$HasTlsKey = -not [string]::IsNullOrWhiteSpace($TlsKey)
if ($HasTlsCert -ne $HasTlsKey) {
    throw "Both -TlsCert and -TlsKey must be specified together to enable TLS."
}

$Exe = ""
if (-not [string]::IsNullOrWhiteSpace($ExecutablePath)) {
    $FullExe = [IO.Path]::GetFullPath($ExecutablePath)
    if (!(Test-Path -LiteralPath $FullExe -PathType Leaf)) {
        throw "Configured aircd executable not found: $FullExe"
    }
    $Exe = $FullExe
} else {
    $DebugExe = Join-Path $RepoRoot "target\debug\aircd.exe"
    $Exe = & (Join-Path $PSScriptRoot "Get-AircdExecutable.ps1") -RepoRoot $RepoRoot
    if (!$Exe) {
        Write-Host "Building aircd..."
        & cargo build --manifest-path (Join-Path $RepoRoot "Cargo.toml")
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        $Exe = $DebugExe
    }
}

$ArgList = @("--bind", $Bind, "--http-bind", $HttpBind, "--db", $DbPath)
if ($OpenRegistration) { $ArgList += "--open-registration" }
if (-not [string]::IsNullOrWhiteSpace($RegisterKey)) { $ArgList += @("--register-key", $RegisterKey) }
if ($HasTlsCert) {
    $ArgList += @("--tls-cert", [IO.Path]::GetFullPath($TlsCert), "--tls-key", [IO.Path]::GetFullPath($TlsKey), "--tls-bind", $TlsBind)
}
if ($WindowsEventLog) { $ArgList += "--windows-event-log" }
if (-not [string]::IsNullOrWhiteSpace($WindowsEventLogSource)) { $ArgList += @("--windows-event-log-source", $WindowsEventLogSource) }
if (-not [string]::IsNullOrWhiteSpace($SyslogTarget)) { $ArgList += @("--syslog-target", $SyslogTarget) }
if (-not [string]::IsNullOrWhiteSpace($SyslogFormat)) { $ArgList += @("--syslog-format", $SyslogFormat) }
if (-not [string]::IsNullOrWhiteSpace($SyslogFacility)) { $ArgList += @("--syslog-facility", $SyslogFacility) }
if (-not $BackupEnabled) { $ArgList += "--no-backups" }
if (-not [string]::IsNullOrWhiteSpace($BackupDir)) { $ArgList += @("--backup-dir", $BackupDir) }
if (-not [string]::IsNullOrWhiteSpace($BackupIntervalSecs)) { $ArgList += @("--backup-interval-secs", $BackupIntervalSecs) }
if (-not [string]::IsNullOrWhiteSpace($BackupRetention)) { $ArgList += @("--backup-retention", $BackupRetention) }
if ($BackupOnStartup) { $ArgList += "--backup-on-startup" }
if ($Background) {
    $LogDir = if ($DbDir) { $DbDir } else { $RepoRoot }
    $LogOut = Join-Path $LogDir "aircd.stdout.log"
    $LogErr = Join-Path $LogDir "aircd.stderr.log"
    $Proc = Start-Process -FilePath $Exe -ArgumentList $ArgList -RedirectStandardOutput $LogOut -RedirectStandardError $LogErr -PassThru -WindowStyle Hidden
    $DisplayHttpHost = if ($HttpBind.StartsWith("[")) { "[::1]" } elseif ($HttpBind.StartsWith("0.0.0.0:")) { "127.0.0.1" } else { ($HttpBind -split ':')[0] }
    $DisplayIrcHost = if ($Bind.StartsWith("[")) { "[::1]" } elseif ($Bind.StartsWith("0.0.0.0:")) { "127.0.0.1" } else { ($Bind -split ':')[0] }
    Write-Host "Started aircd (PID $($Proc.Id))"
    Write-Host "Welcome/API: http://${DisplayHttpHost}:$(($HttpBind -split ':')[-1])/"
    Write-Host "IRC TCP:     ${DisplayIrcHost}:$(($Bind -split ':')[-1])"
    Write-Host "Logs:        $LogOut"
} else {
    & $Exe @ArgList
    exit $LASTEXITCODE
}
