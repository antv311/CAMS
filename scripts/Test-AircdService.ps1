param(
    [string]$AircdExe = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($AircdExe)) {
    $AircdExe = & (Join-Path $PSScriptRoot 'Get-AircdExecutable.ps1') -RepoRoot $RepoRoot
}
if ([string]::IsNullOrWhiteSpace($AircdExe) -or -not (Test-Path -LiteralPath $AircdExe -PathType Leaf)) {
    throw 'Build aircd first (cargo build) or pass -AircdExe.'
}

Import-Module (Join-Path $PSScriptRoot 'AircdService.psm1') -Force
Initialize-AircdServiceNative

$TempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$TestRoot = Join-Path $TempRoot ('aircd service test ' + [guid]::NewGuid())
$TaskName = 'CAMS-Aircd-Test-' + ([guid]::NewGuid().ToString('N').Substring(0, 12))
$Config = $null
$Unrelated = $null
$OtherProc = $null

$SavedEnv = @{}
foreach ($EnvName in @('AIRCD_CONFIG_DIR', 'AIRCD_DATA_DIR', 'AIRCD_CONFIG', 'AIRCD_WINDOWS_CONFIG', 'AIRCD_EXECUTABLE', 'AIRCD_BIND', 'AIRCD_HTTP_BIND', 'AIRCD_DB', 'AIRCD_OPEN_REGISTRATION', 'AIRCD_REGISTER_KEY', 'AIRCD_TLS_CERT', 'AIRCD_TLS_KEY', 'AIRCD_TLS_BIND', 'AIRCD_SERVICE_NAME', 'AIRCD_TASK_PATH', 'AIRCD_SERVICE_MODE', 'AIRCD_SERVICE_TRIGGER')) {
    $Item = Get-Item -LiteralPath "Env:\$EnvName" -ErrorAction SilentlyContinue
    $SavedEnv[$EnvName] = if ($null -ne $Item) { $Item.Value } else { $null }
    Remove-Item -LiteralPath "Env:\$EnvName" -ErrorAction SilentlyContinue
}

function Assert($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Assert-Throws([scriptblock]$Action, [string]$Pattern) {
    try {
        & $Action
    } catch {
        Assert ($_.Exception.Message -like "*$Pattern*") "Unexpected error: $($_.Exception.Message)"
        return
    }
    throw "Expected failure matching '*$Pattern*'."
}

function Get-FreePort {
    $Listener = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, 0)
    $Listener.Start()
    $Port = $Listener.LocalEndpoint.Port
    $Listener.Stop()
    $Port
}

function Invoke-InstallerCli {
    param([string[]]$Arguments)
    $PrevPref = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $Raw = & powershell.exe @Arguments 2>&1
        $Code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $PrevPref
    }
    [pscustomobject]@{
        ExitCode = $Code
        Output   = (($Raw | ForEach-Object { "$_" }) -join "`n").Trim()
    }
}

function Stop-TestProcesses {
    param([string]$ImageName, [string]$RootPrefix)
    $Matches = @(Get-CimInstance Win32_Process -Filter "Name = '$ImageName'" -ErrorAction SilentlyContinue | Where-Object {
        $_.ExecutablePath -and $_.ExecutablePath.StartsWith($RootPrefix, [StringComparison]::OrdinalIgnoreCase)
    })
    foreach ($ProcInfo in $Matches) {
        $P = Get-Process -Id $ProcInfo.ProcessId -ErrorAction SilentlyContinue
        if (-not $P) { continue }
        try {
            if (-not $P.HasExited) {
                try {
                    $P.Kill()
                } catch [System.InvalidOperationException] {
                } catch [System.ComponentModel.Win32Exception] {
                    # Process may already be terminating (Access Denied during teardown).
                }
            }
            $null = $P.WaitForExit(10000)
        } catch {
        } finally {
            $P.Dispose()
        }
    }
}

try {
    New-Item -ItemType Directory -Path `
        (Join-Path $TestRoot 'scripts'), `
        (Join-Path $TestRoot 'isolated-config'), `
        (Join-Path $TestRoot 'target\release'), `
        (Join-Path $TestRoot 'target\debug') -Force | Out-Null
    $env:AIRCD_CONFIG_DIR = Join-Path $TestRoot 'isolated-config'
    $env:AIRCD_DATA_DIR = Join-Path $TestRoot 'isolated-data'
    Copy-Item -Path (Join-Path $PSScriptRoot '*.ps1'), (Join-Path $PSScriptRoot '*.psm1') -Destination (Join-Path $TestRoot 'scripts')

    $Release = Join-Path $TestRoot 'target\release\aircd.exe'
    $Debug = Join-Path $TestRoot 'target\debug\aircd.exe'
    Copy-Item -LiteralPath $AircdExe -Destination $Release

    $IrcPort = Get-FreePort
    do { $HttpPort = Get-FreePort } while ($HttpPort -eq $IrcPort)
    $Db = Join-Path $TestRoot 'data folder\hub.sqlite3'
    $Installer = Join-Path $TestRoot 'scripts\Install-AircdService.ps1'

    # 1. Argument quoting and config validation.
    $Values = @('fixture.exe', '', 'ordinary', 'with spaces', 'literal"quote', 'C:\trailing slash\', 'slash\"quote', '$dollar`tick')
    $Parsed = [AircdService.Native]::Split((Join-AircdServiceArguments $Values))
    Assert (($Parsed | ConvertTo-Json -Compress) -ceq ($Values | ConvertTo-Json -Compress)) 'Windows argument quoting roundtrip failed.'

    $Config = New-AircdServiceConfig -RepoRoot $TestRoot -Name $TaskName -Bind "127.0.0.1:$IrcPort" -HttpBind "127.0.0.1:$HttpPort" -DbPath $Db
    $TlsCertFixture = Join-Path $TestRoot 'cert.pem'
    $TlsKeyFixture = Join-Path $TestRoot 'key.pem'
    Set-Content -LiteralPath $TlsCertFixture -Value 'cert'
    Set-Content -LiteralPath $TlsKeyFixture -Value 'key'
    $CustomCfg = New-AircdServiceConfig -RepoRoot $TestRoot -Name $TaskName -Bind "127.0.0.1:$IrcPort" -HttpBind "127.0.0.1:$HttpPort" -DbPath $Db -OpenRegistration -RegisterKey 'secret-123' -TlsCert $TlsCertFixture -TlsKey $TlsKeyFixture
    Assert ($CustomCfg.ServerArguments -contains '--open-registration' -and $CustomCfg.ServerArguments -contains '--register-key' -and $CustomCfg.ServerArguments -contains '--tls-cert') 'Optional server flags were not propagated.'
    Remove-Item -LiteralPath $TlsCertFixture, $TlsKeyFixture -Force
    Assert-Throws { New-AircdServiceConfig -RepoRoot $TestRoot -Bind 'remote:6667' } 'local numeric'
    Assert-Throws { New-AircdServiceConfig -RepoRoot $TestRoot -Bind "127.0.0.1:$HttpPort" -HttpBind "127.0.0.1:$HttpPort" } 'different ports'
    Assert-Throws { New-AircdServiceConfig -RepoRoot $TestRoot -DbPath ':memory:' } 'persistent database'
    Assert-Throws { New-AircdServiceConfig -RepoRoot $TestRoot -Name 'bad/name' } 'Invalid service/task name'
    Assert-Throws { New-AircdServiceConfig -RepoRoot $TestRoot -TlsCert 'only-cert.pem' } 'together'
    $MissingBuildCfg = New-AircdServiceConfig -RepoRoot (Join-Path $TestRoot 'no-build') -Name $TaskName -Bind "127.0.0.1:$IrcPort" -HttpBind "127.0.0.1:$HttpPort" -DbPath $Db
    Assert-Throws { Resolve-AircdServiceExecutable $MissingBuildCfg } 'No aircd build found'

    # 1b. Editable config file + env + CLI precedence for service installer.
    $SvcCfgFile = Join-Path $TestRoot 'service-config.json'
    $SvcFileDb = Join-Path $TestRoot 'cfg-data\service-from-file.sqlite3'
    @{
        server = @{
            bind              = "127.0.0.1:$IrcPort"
            http_bind         = "127.0.0.1:$HttpPort"
            db_path           = 'cfg-data\service-from-file.sqlite3'
            open_registration = $true
        }
        service = @{
            name = "$TaskName-File"
        }
    } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $SvcCfgFile -Encoding UTF8
    $FromFileCfg = New-AircdServiceConfig -RepoRoot $TestRoot -ConfigPath $SvcCfgFile
    Assert ($FromFileCfg.Name -eq "$TaskName-File" -and $FromFileCfg.DbPath -eq $SvcFileDb -and ($FromFileCfg.ServerArguments -contains '--open-registration')) 'Service config file settings were not loaded.'
    $env:AIRCD_SERVICE_NAME = "$TaskName-Env"
    $FromEnvStatus = (& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Installer -ConfigPath $SvcCfgFile -Status -Json) -join "`n" | ConvertFrom-Json
    Assert ($FromEnvStatus.name -eq "$TaskName-Env" -and $FromEnvStatus.db_path -eq $SvcFileDb -and $FromEnvStatus.bind -eq "127.0.0.1:$IrcPort") 'Install-AircdService.ps1 did not honor Env > ConfigFile precedence.'
    $FromCliStatus = (& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Installer -ConfigPath $SvcCfgFile -Name "$TaskName-Cli" -Status -Json) -join "`n" | ConvertFrom-Json
    Assert ($FromCliStatus.name -eq "$TaskName-Cli") 'Install-AircdService.ps1 did not honor CLI > Env precedence.'
    Remove-Item Env:\AIRCD_SERVICE_NAME -ErrorAction SilentlyContinue

    # 2. Elevation & install plan matrix (simulated elevated + non-elevated).
    $NonElevAuto = Resolve-AircdServiceInstallPlan -Mode Auto -Trigger Auto -IsElevated $false
    Assert ($NonElevAuto.Mode -eq 'ScheduledTask' -and $NonElevAuto.Trigger -eq 'AtLogOn' -and $NonElevAuto.LogonType -eq 'Interactive' -and -not $NonElevAuto.SurvivesLogout) 'Unexpected non-elevated Auto plan.'

    $ElevAuto = Resolve-AircdServiceInstallPlan -Mode Auto -Trigger Auto -IsElevated $true
    Assert ($ElevAuto.Mode -eq 'ScheduledTask' -and $ElevAuto.Trigger -eq 'AtStartup' -and $ElevAuto.LogonType -eq 'S4U' -and $ElevAuto.SurvivesLogout -and $ElevAuto.SurvivesReboot) 'Unexpected elevated Auto plan.'

    $ElevService = Resolve-AircdServiceInstallPlan -Mode Service -IsElevated $true
    Assert ($ElevService.Mode -eq 'Service' -and $ElevService.LogonType -eq 'LocalSystem' -and $ElevService.SurvivesLogout) 'Unexpected elevated Service plan.'

    $ElevSystemTask = Resolve-AircdServiceInstallPlan -Mode ScheduledTask -RunAsSystem -IsElevated $true
    Assert ($ElevSystemTask.Mode -eq 'ScheduledTask' -and $ElevSystemTask.Trigger -eq 'AtStartup' -and $ElevSystemTask.LogonType -eq 'ServiceAccount') 'Unexpected elevated System task plan.'

    Assert-Throws { Resolve-AircdServiceInstallPlan -Mode Service -IsElevated $false } 'Administrator'
    Assert-Throws { Resolve-AircdServiceInstallPlan -Mode ScheduledTask -SurviveLogout -IsElevated $false } 'Administrator'
    Assert-Throws { Resolve-AircdServiceInstallPlan -Mode ScheduledTask -Trigger AtStartup -IsElevated $false } 'Administrator'
    Assert-Throws { Resolve-AircdServiceInstallPlan -Mode ScheduledTask -RunAsSystem -IsElevated $false } 'Administrator'

    # Verify CLI rejects mutually exclusive action switches and non-elevated -Mode Service.
    $FailRes = Invoke-InstallerCli @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Installer, '-Name', $TaskName, '-Start', '-Stop')
    Assert ($FailRes.ExitCode -ne 0 -and $FailRes.Output -like '*Specify at most one*') 'Mutually exclusive action switches should fail.'
    if (-not (Test-AircdServiceIsElevated)) {
        $SvcRes = Invoke-InstallerCli @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Installer, '-Name', $TaskName, '-Bind', $Config.Bind, '-HttpBind', $Config.HttpBind, '-DbPath', $Config.DbPath, '-Mode', 'Service')
        Assert ($SvcRes.ExitCode -ne 0 -and $SvcRes.Output -like '*Administrator*') 'Non-elevated -Mode Service should fail with elevation guidance.'
    }

    # 3. Real Scheduled Task install + start from a separate PowerShell process that exits immediately.
    $InstallArgs = @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Installer,
        '-Mode', 'ScheduledTask', '-Name', $TaskName,
        '-Bind', $Config.Bind, '-HttpBind', $Config.HttpBind, '-DbPath', $Config.DbPath,
        '-StartNow', '-Json'
    )
    $InstallJson = & powershell.exe @InstallArgs
    Assert ($LASTEXITCODE -eq 0) "Install-AircdService.ps1 -StartNow failed: $InstallJson"
    $InstallStatus = ($InstallJson -join "`n") | ConvertFrom-Json
    Assert ($InstallStatus.installed -and $InstallStatus.hub_state -eq 'Running' -and $InstallStatus.mode -eq 'ScheduledTask') 'Installed hub did not report Running.'
    $FirstPid = [int]$InstallStatus.pid
    Assert ($InstallStatus.executable -ieq $Release) "Initial launch did not pick release build (got $($InstallStatus.executable))."

    # Confirm Task Scheduler task settings and parent independence.
    $Task = Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop
    Assert ($Task.Settings.ExecutionTimeLimit -eq 'PT0S') 'ExecutionTimeLimit must be unlimited (PT0S).'
    Assert ($Task.Settings.MultipleInstances -eq 'IgnoreNew') 'MultipleInstances must be IgnoreNew.'
    Assert ($Task.Settings.RestartCount -eq 3) 'RestartCount should be 3.'
    Assert (-not $Task.Settings.DisallowStartIfOnBatteries -and -not $Task.Settings.StopIfGoingOnBatteries) 'Battery stop flags should be disabled.'

    $ChildInfo = Get-CimInstance Win32_Process -Filter "ProcessId = $FirstPid"
    $HostInfo = Get-CimInstance Win32_Process -Filter "ProcessId = $($ChildInfo.ParentProcessId)"
    $HostParentInfo = Get-CimInstance Win32_Process -Filter "ProcessId = $($HostInfo.ParentProcessId)"
    Assert ([IO.Path]::GetFileName($HostInfo.ExecutablePath) -ieq 'aircd-service-host.exe') 'aircd.exe was not launched by aircd-service-host.exe.'
    Assert ($HostParentInfo.Name -ieq 'svchost.exe') "Expected service host parent svchost.exe, got $($HostParentInfo.Name)."

    # Confirm logs and live HTTP persistence.
    Assert (Test-Path -LiteralPath (Join-Path $Config.LogDir 'aircd.stdout.log')) 'aircd.stdout.log missing.'
    Assert (Test-Path -LiteralPath (Join-Path $Config.LogDir 'aircd.stderr.log')) 'aircd.stderr.log missing.'
    Assert (Test-Path -LiteralPath (Join-Path $Config.LogDir 'aircd.service.log')) 'aircd.service.log missing.'
    $OwnerKeyPath = Join-Path $Config.LogDir 'aircd-owner.key'
    Assert (Test-Path -LiteralPath $OwnerKeyPath) 'aircd-owner.key missing beside DB.'
    $OwnerKey = (Get-Content -LiteralPath $OwnerKeyPath -Raw).Trim()
    $Registration = Invoke-RestMethod -UseBasicParsing -Uri "$($Config.Url)/v1/register" -Method Post -Headers @{ 'X-AirCD-Invite' = $OwnerKey } -ContentType 'application/json' -Body '{"nick":"service-test","role":"worker"}'
    Assert ($Registration.token) 'Registration via installed hub failed.'

    # 4. Idempotent re-install while running preserves running state and DB.
    $ReinstallJson = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Installer `
        -Mode ScheduledTask -Name $TaskName -Bind $Config.Bind -HttpBind $Config.HttpBind -DbPath $Config.DbPath -Json
    Assert ($LASTEXITCODE -eq 0) "Idempotent reinstall failed: $ReinstallJson"
    $ReinstallStatus = ($ReinstallJson -join "`n") | ConvertFrom-Json
    Assert ($ReinstallStatus.hub_state -eq 'Running') 'Hub should be Running after idempotent reinstall.'

    # 5. Restart picks up newer debug build and preserves DB state.
    Copy-Item -LiteralPath $AircdExe -Destination $Debug
    (Get-Item -LiteralPath $Debug).LastWriteTimeUtc = [datetime]::UtcNow.AddMinutes(2)
    $RestartJson = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Installer `
        -Name $TaskName -Bind $Config.Bind -HttpBind $Config.HttpBind -DbPath $Config.DbPath -Restart -Json
    Assert ($LASTEXITCODE -eq 0) "Restart failed: $RestartJson"
    $RestartStatus = ($RestartJson -join "`n") | ConvertFrom-Json
    Assert ($RestartStatus.hub_state -eq 'Running') 'Hub not running after restart.'
    Assert ([int]$RestartStatus.pid -ne $FirstPid) 'Restart did not replace the running process.'
    Assert ($RestartStatus.executable -ieq $Debug) "Restart did not select newer debug build (got $($RestartStatus.executable))."
    $Agents = Invoke-RestMethod -UseBasicParsing -Uri "$($Config.Url)/v1/agents"
    Assert (($Agents | ConvertTo-Json -Depth 8) -match 'service-test') 'SQLite state did not persist across restart.'

    # 6. Mismatched DB protection: refuse to stop a hub running with a different DB.
    $OtherConfig = New-AircdServiceConfig -RepoRoot $TestRoot -Name $TaskName -Bind $Config.Bind -HttpBind $Config.HttpBind -DbPath (Join-Path $TestRoot 'other.sqlite3')
    Assert-Throws { Stop-AircdServiceInstance -Config $OtherConfig } 'different bind/database'
    Assert ((Test-AircdServiceHealth $Config).State -eq 'Running') 'Mismatched config stop affected the running hub.'

    # 7. Clean -Stop releases ports and leaves task ready.
    $StopJson = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Installer `
        -Name $TaskName -Bind $Config.Bind -HttpBind $Config.HttpBind -DbPath $Config.DbPath -Stop -Json
    Assert ($LASTEXITCODE -eq 0) "Stop failed: $StopJson"
    $StopStatus = ($StopJson -join "`n") | ConvertFrom-Json
    Assert ($StopStatus.hub_state -eq 'Stopped' -and @(Get-AircdServiceListeners $Config).Count -eq 0) 'Stop did not stop hub or release ports.'
    $TaskAfterStop = Get-ScheduledTask -TaskName $TaskName
    Assert ($TaskAfterStop.State -ne 'Running') 'Scheduled task remained Running after -Stop.'

    # 8. Unrelated listener protection on -Start, then clean -Start and human -Status.
    $Unrelated = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, $Config.HttpPort)
    $Unrelated.Start()
    $ErrRes = Invoke-InstallerCli @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Installer, '-Name', $TaskName, '-Bind', $Config.Bind, '-HttpBind', $Config.HttpBind, '-DbPath', $Config.DbPath, '-Start')
    Assert ($ErrRes.ExitCode -ne 0 -and $ErrRes.Output -like '*unrecognized process*') "Expected unrecognized process error when port is occupied; got: $($ErrRes.Output)"
    Assert (@(Get-AircdServiceListeners $Config).Count -eq 1) 'Unrelated listener was disturbed.'
    $Unrelated.Stop()
    $Unrelated = $null

    $StartRes = Invoke-InstallerCli @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Installer, '-Name', $TaskName, '-Bind', $Config.Bind, '-HttpBind', $Config.HttpBind, '-DbPath', $Config.DbPath, '-Start')
    Assert ($StartRes.ExitCode -eq 0 -and $StartRes.Output -like "*Started '$TaskName'*") "Start after stop failed: $($StartRes.Output)"
    $HumanStatus = Invoke-InstallerCli @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Installer, '-Name', $TaskName, '-Bind', $Config.Bind, '-HttpBind', $Config.HttpBind, '-DbPath', $Config.DbPath, '-Status')
    Assert ($HumanStatus.ExitCode -eq 0 -and $HumanStatus.Output -like '*Installed:*True*' -and $HumanStatus.Output -like '*Hub:*Running*') "Human status output unexpected: $($HumanStatus.Output)"
    $StopAgainRes = Invoke-InstallerCli @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Installer, '-Name', $TaskName, '-Bind', $Config.Bind, '-HttpBind', $Config.HttpBind, '-DbPath', $Config.DbPath, '-Stop')
    Assert ($StopAgainRes.ExitCode -eq 0) "Second stop failed: $($StopAgainRes.Output)"

    # 9. Verify Windows Service C# host class compiles and behaves cleanly in-process.
    $CheckHostPath = $Config.HostExePath
    Assert (Test-Path -LiteralPath $CheckHostPath -PathType Leaf) 'aircd-service-host.exe was not built.'
    $Asm = [Reflection.Assembly]::Load([IO.File]::ReadAllBytes($CheckHostPath))
    $SvcType = $Asm.GetType('AircdServiceHost.AircdWindowsService')
    $RunnerType = $Asm.GetType('AircdServiceHost.HostRunner')
    $ArgListInstance = [System.Collections.Generic.List[string]]::new()
    $CtorArgs = [object[]]::new(5)
    $CtorArgs[0] = [string]$TaskName
    $CtorArgs[1] = [string]$TestRoot
    $CtorArgs[2] = [string]$Config.LogDir
    $CtorArgs[3] = [string]''
    $CtorArgs[4] = $ArgListInstance
    $SvcInstance = $SvcType.GetConstructors()[0].Invoke($CtorArgs)
    Assert ($SvcInstance.ServiceName -eq $TaskName -and $SvcInstance.CanStop -and $SvcInstance.CanShutdown) 'AircdWindowsService properties mismatch.'
    $SvcInstance.Dispose()
    $ResolvedByHost = $RunnerType.GetMethod('ResolveExecutable').Invoke($null, [object[]]@([string]$TestRoot, [string]''))
    Assert ($ResolvedByHost -ieq $Debug) 'C# HostRunner.ResolveExecutable did not select newest build.'
    (Get-Item -LiteralPath $Debug).LastWriteTimeUtc = (Get-Item -LiteralPath $Release).LastWriteTimeUtc
    $ResolvedTie = $RunnerType.GetMethod('ResolveExecutable').Invoke($null, [object[]]@([string]$TestRoot, [string]''))
    Assert ($ResolvedTie -ieq $Release) 'C# HostRunner.ResolveExecutable tie-break did not prefer release.'

    # 10. Uninstall removes task and host artifacts while preserving DB and logs.
    $UninstallJson = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Installer `
        -Name $TaskName -Bind $Config.Bind -HttpBind $Config.HttpBind -DbPath $Config.DbPath -Uninstall -Json
    Assert ($LASTEXITCODE -eq 0) "Uninstall failed: $UninstallJson"
    $UninstallObj = ($UninstallJson -join "`n") | ConvertFrom-Json
    Assert ($UninstallObj.removed) 'Uninstall did not report removed = true.'
    Assert (-not (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue)) 'Scheduled task still exists after uninstall.'
    Assert (-not (Test-Path -LiteralPath $Config.ServiceDir)) 'Service directory was not cleaned up on uninstall.'
    Assert (Test-Path -LiteralPath $Config.DbPath) 'Database was deleted during uninstall.'
    Assert (Test-Path -LiteralPath (Join-Path $Config.LogDir 'aircd.stdout.log')) 'Logs were deleted during uninstall.'

    # 11. -Status after uninstall reports installed = false.
    $PostStatus = (& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Installer `
        -Name $TaskName -Bind $Config.Bind -HttpBind $Config.HttpBind -DbPath $Config.DbPath -Status -Json) -join "`n" | ConvertFrom-Json
    Assert (-not $PostStatus.installed -and $PostStatus.hub_state -eq 'Stopped') 'Post-uninstall status incorrect.'

    Write-Host 'PASS: service config validation, elevation guards, Windows Service host class, real Scheduled Task lifecycle (start, session-independent svchost parent, persistence, idempotent reinstall, newest-build restart, port protection, stop, and uninstall preserving DB/logs).'
} finally {
    foreach ($EnvName in $SavedEnv.Keys) {
        if ($null -eq $SavedEnv[$EnvName]) {
            Remove-Item -LiteralPath "Env:\$EnvName" -ErrorAction SilentlyContinue
        } else {
            Set-Item -LiteralPath "Env:\$EnvName" -Value $SavedEnv[$EnvName]
        }
    }
    if ($Unrelated) { try { $Unrelated.Stop() } catch {} }
    if ($OtherProc) {
        try {
            if (-not $OtherProc.HasExited) { $OtherProc.Kill() }
        } catch {}
    }
    if ($Config) {
        try {
            $ExistingTask = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
            if ($ExistingTask) {
                Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
                Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
            }
        } catch {}
        Stop-TestProcesses -ImageName 'aircd-service-host.exe' -RootPrefix ($TestRoot + '\')
        Stop-TestProcesses -ImageName 'aircd.exe' -RootPrefix ($TestRoot + '\')
    }
    $ResolvedTestRoot = [IO.Path]::GetFullPath($TestRoot)
    if (-not $ResolvedTestRoot.StartsWith($TempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        (Split-Path -Leaf $ResolvedTestRoot) -notlike 'aircd service test *') {
        throw 'Refusing unexpected test cleanup path.'
    }
    if (Test-Path -LiteralPath $ResolvedTestRoot) {
        for ($Attempt = 1; $Attempt -le 10; $Attempt++) {
            try {
                Remove-Item -LiteralPath $ResolvedTestRoot -Recurse -Force -ErrorAction Stop
                break
            } catch {
                if ($Attempt -ge 10) { throw }
                Start-Sleep -Milliseconds 150
            }
        }
    }
}
