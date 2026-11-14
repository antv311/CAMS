<#
.SYNOPSIS
Installs, configures, and manages aircd as an autonomous Windows Scheduled Task or Windows Service.

.DESCRIPTION
Configures the CAMS aircd coordination hub to run independently of any terminal
or assistant session and survive system restarts (and user logout when installed
from an elevated PowerShell prompt).

By default (-Mode Auto):
- When elevated (Administrator): installs an AtStartup Scheduled Task running as
  the current user with LogonType S4U (survives restarts and user logout without
  storing a password, while preserving user profile and agent CLI paths).
- When non-elevated: installs an AtLogOn Scheduled Task running hidden in the
  current user's context (survives closing terminals/assistant sessions and
  restarts on sign-in).

Use -Mode Service (requires Administrator) to install a true Windows Service
with automatic startup and SCM crash recovery.
#>
param(
    [string]$ConfigPath = '',

    [ValidateSet('Auto', 'ScheduledTask', 'Service')]
    [string]$Mode = 'Auto',

    [ValidateSet('Auto', 'AtStartup', 'AtLogOn')]
    [string]$Trigger = 'Auto',

    [string]$Name = 'CAMS-Aircd',
    [string]$TaskPath = '\',
    [string]$Bind = '127.0.0.1:6667',
    [string]$HttpBind = '127.0.0.1:6680',
    [string]$DbPath = '',
    [switch]$OpenRegistration,
    [string]$RegisterKey = '',
    [string]$TlsCert = '',
    [string]$TlsKey = '',
    [string]$TlsBind = '127.0.0.1:6697',
    [switch]$WindowsEventLog,
    [string]$WindowsEventLogSource = '',
    [string]$SyslogTarget = '',
    [string]$SyslogFormat = '',
    [string]$SyslogFacility = '',
    [string]$ExecutablePath = '',

    [switch]$RunAsSystem,
    [pscredential]$Credential,
    [switch]$SurviveLogout,

    [switch]$StartNow,
    [switch]$Uninstall,
    [switch]$Start,
    [switch]$Stop,
    [switch]$Restart,
    [switch]$Status,
    [switch]$Json
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ActionFlags = @(@(
    @{ Name = 'Uninstall'; Set = [bool]$Uninstall },
    @{ Name = 'Start';     Set = [bool]$Start },
    @{ Name = 'Stop';      Set = [bool]$Stop },
    @{ Name = 'Restart';   Set = [bool]$Restart },
    @{ Name = 'Status';    Set = [bool]$Status }
) | Where-Object { $_.Set })

if ($ActionFlags.Count -gt 1) {
    $Names = ($ActionFlags | ForEach-Object { "-$($_.Name)" }) -join ', '
    throw "Specify at most one action switch ($Names)."
}
if ($StartNow -and ($Uninstall -or $Stop -or $Status)) {
    throw '-StartNow cannot be combined with -Uninstall, -Stop, or -Status.'
}

Import-Module (Join-Path $PSScriptRoot 'AircdService.psm1') -Force
$RepoRoot = Split-Path -Parent $PSScriptRoot
$CfgParams = @{ RepoRoot = $RepoRoot }
foreach ($Key in @('ConfigPath', 'Name', 'TaskPath', 'Bind', 'HttpBind', 'DbPath', 'OpenRegistration', 'RegisterKey', 'TlsCert', 'TlsKey', 'TlsBind', 'WindowsEventLog', 'WindowsEventLogSource', 'SyslogTarget', 'SyslogFormat', 'SyslogFacility', 'ExecutablePath')) {
    if ($PSBoundParameters.ContainsKey($Key)) {
        $CfgParams[$Key] = $PSBoundParameters[$Key]
    }
}
$Config = New-AircdServiceConfig @CfgParams
if (-not $PSBoundParameters.ContainsKey('Mode')) {
    if ($Config.Mode -notin @('Auto', 'ScheduledTask', 'Service')) {
        throw "Invalid configured service mode '$($Config.Mode)'. Expected Auto, ScheduledTask, or Service."
    }
    $Mode = $Config.Mode
}
if (-not $PSBoundParameters.ContainsKey('Trigger')) {
    if ($Config.Trigger -notin @('Auto', 'AtStartup', 'AtLogOn')) {
        throw "Invalid configured service trigger '$($Config.Trigger)'. Expected Auto, AtStartup, or AtLogOn."
    }
    $Trigger = $Config.Trigger
}

if ($Status) {
    $Report = Get-AircdServiceStatusReport -Config $Config -Mode $Mode
    if ($Json) {
        $Report | ConvertTo-Json -Depth 5
    } else {
        Write-Host "Name:            $($Report.name)"
        Write-Host "Installed:       $($Report.installed)"
        if ($Report.installed) {
            Write-Host "Mode:            $($Report.mode) ($($Report.installer_state))"
            Write-Host "Trigger/Logon:   $($Report.trigger) / $($Report.logon_type)"
            Write-Host "Survives logout: $($Report.survives_logout)"
        }
        Write-Host "Hub:             $($Report.hub_state) - $($Report.hub_detail)"
        if ($Report.pid) {
            Write-Host "PID:             $($Report.pid)"
            Write-Host "Executable:      $($Report.executable)"
        }
        Write-Host "URL:             $($Report.url) (IRC: $($Report.bind))"
        Write-Host "Database:        $($Report.db_path)"
        Write-Host "Logs:            $($Report.log_dir)"
    }
    exit 0
}

if ($Uninstall) {
    $Removed = Uninstall-AircdServiceInstance -Config $Config -Mode $Mode
    if ($Json) {
        [pscustomobject]@{
            ok      = $true
            action  = 'uninstall'
            name    = $Config.Name
            removed = [bool]$Removed.Installed
            mode    = $Removed.Mode
        } | ConvertTo-Json -Depth 4
    } else {
        if ($Removed.Installed) {
            Write-Host "Uninstalled $($Removed.Mode) '$($Config.Name)'. Database and logs were preserved in $($Config.LogDir)."
        } else {
            Write-Host "No service or scheduled task named '$($Config.Name)' was installed."
        }
    }
    exit 0
}

if ($Stop) {
    Stop-AircdServiceInstance -Config $Config -Mode $Mode
    $Report = Get-AircdServiceStatusReport -Config $Config -Mode $Mode
    if ($Json) {
        $Report | ConvertTo-Json -Depth 5
    } else {
        Write-Host "Stopped '$($Config.Name)' ($($Report.hub_detail))."
    }
    exit 0
}

if ($Start) {
    $Ready = Start-AircdServiceInstance -Config $Config -Mode $Mode
    $Report = Get-AircdServiceStatusReport -Config $Config -Mode $Mode
    if ($Json) {
        $Report | ConvertTo-Json -Depth 5
    } else {
        Write-Host "Started '$($Config.Name)' (PID $($Ready.Server.Id)) at $($Config.Url)."
    }
    exit 0
}

if ($Restart) {
    Stop-AircdServiceInstance -Config $Config -Mode $Mode
    $Ready = Start-AircdServiceInstance -Config $Config -Mode $Mode
    $Report = Get-AircdServiceStatusReport -Config $Config -Mode $Mode
    if ($Json) {
        $Report | ConvertTo-Json -Depth 5
    } else {
        Write-Host "Restarted '$($Config.Name)' (PID $($Ready.Server.Id)) at $($Config.Url)."
    }
    exit 0
}

$InstallResult = Install-AircdServiceInstance `
    -Config $Config `
    -Mode $Mode `
    -Trigger $Trigger `
    -RunAsSystem:$RunAsSystem `
    -Credential $Credential `
    -SurviveLogout:$SurviveLogout

$Started = $null
if ($StartNow -or $InstallResult.WasRunning) {
    $Started = Start-AircdServiceInstance -Config $Config -Mode $InstallResult.Plan.Mode
}

$Report = Get-AircdServiceStatusReport -Config $Config -Mode $InstallResult.Plan.Mode
if ($Json) {
    $Report | ConvertTo-Json -Depth 5
} else {
    Write-Host "Installed $($InstallResult.Plan.Mode) '$($Config.Name)' ($($InstallResult.Plan.Trigger) / $($InstallResult.Plan.LogonType))."
    Write-Host "Hub URL:  $($Config.Url) (IRC: $($Config.Bind))"
    Write-Host "Database: $($Config.DbPath)"
    Write-Host "Logs:     $($Config.LogDir)\aircd.stdout.log"
    if ($Started) {
        Write-Host "Running:  PID $($Started.Server.Id) ($($Started.Server.Executable))"
        $ServerProcInfo = Get-CimInstance Win32_Process -Filter "ProcessId = $($Started.Server.Id)" -ErrorAction SilentlyContinue
        $ParentProcInfo = if ($ServerProcInfo) {
            Get-CimInstance Win32_Process -Filter "ProcessId = $($ServerProcInfo.ParentProcessId)" -ErrorAction SilentlyContinue
        } else { $null }
        if (-not $ParentProcInfo -or [IO.Path]::GetFileName($ParentProcInfo.ExecutablePath) -ine 'aircd-service-host.exe') {
            Write-Host "NOTE: Existing hub PID $($Started.Server.Id) is running outside the service/task host. Run .\scripts\Install-AircdService.ps1 -Name $($Config.Name) -Restart to move it under the autonomous host."
        }
    } else {
        Write-Host "Start it now with: .\scripts\Install-AircdService.ps1 -Name $($Config.Name) -Start"
    }
    if (-not $InstallResult.Plan.SurvivesLogout) {
        Write-Host "NOTE: Installed as a non-elevated AtLogOn task (survives terminal/assistant session exit and restarts at sign-in). Run from an elevated Administrator PowerShell prompt (or pass -Mode Service / -SurviveLogout) to start before sign-in and survive user logout."
    }
}
