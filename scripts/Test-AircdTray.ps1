param([string]$AircdExe = (Join-Path (Split-Path -Parent $PSScriptRoot) 'target\debug\aircd.exe'))
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'AircdTray.psm1') -Force
Add-Type -AssemblyName System.Windows.Forms
Initialize-AircdNative
$TempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$TestRoot = Join-Path $TempRoot ('aircd tray test ' + [guid]::NewGuid())
$Config = $null
$Unrelated = $null
$Tui = $null
$SavedEnv = @{}
foreach ($EnvName in @('AIRCD_CONFIG_DIR', 'AIRCD_DATA_DIR', 'AIRCD_CONFIG', 'AIRCD_WINDOWS_CONFIG', 'AIRCD_EXECUTABLE', 'AIRCD_BIND', 'AIRCD_HTTP_BIND', 'AIRCD_DB', 'AIRCD_OPEN_REGISTRATION', 'AIRCD_REGISTER_KEY', 'AIRCD_TLS_CERT', 'AIRCD_TLS_KEY', 'AIRCD_TLS_BIND', 'AIRCD_PROFILE', 'AIRCD_STARTUP_DIR')) {
    $Item = Get-Item -LiteralPath "Env:\$EnvName" -ErrorAction SilentlyContinue
    $SavedEnv[$EnvName] = if ($null -ne $Item) { $Item.Value } else { $null }
    Remove-Item -LiteralPath "Env:\$EnvName" -ErrorAction SilentlyContinue
}
function Assert($Condition, [string]$Message) { if (!$Condition) { throw $Message } }
function Assert-Throws([scriptblock]$Action, [string]$Pattern) {
    try { & $Action } catch {
        Assert ($_.Exception.Message -like "*$Pattern*") "Unexpected error: $($_.Exception.Message)"
        return
    }
    throw "Expected failure: $Pattern"
}
function Get-FreePort {
    $Listener = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, 0)
    $Listener.Start()
    $Port = $Listener.LocalEndpoint.Port
    $Listener.Stop()
    $Port
}
function Wait-TrayLog([string]$Pattern) {
    $Log = Join-Path $Config.LogDir 'aircd.tray.log'
    $Deadline = [datetime]::UtcNow.AddSeconds(20)
    do {
        try {
            if (Test-Path -LiteralPath $Log) {
                $Stream = [System.IO.FileStream]::new($Log, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
                $Reader = [System.IO.StreamReader]::new($Stream, [System.Text.Encoding]::UTF8)
                $Content = $Reader.ReadToEnd()
                $Reader.Close()
                $Stream.Close()
                if ($Content -like "*$Pattern*") { return }
            }
        } catch {}
        Start-Sleep -Milliseconds 200
    } while ([datetime]::UtcNow -lt $Deadline)
    throw "Tray never displayed $Pattern"
}
function Get-TestTrays {
    @(Get-CimInstance Win32_Process -Filter "Name = 'powershell.exe'" | Where-Object {
        $_.CommandLine -and $_.CommandLine.IndexOf((Join-Path $TestRoot 'scripts\Start-AircdTray.ps1'), [StringComparison]::OrdinalIgnoreCase) -ge 0
    })
}
try {
    Assert (Test-Path -LiteralPath $AircdExe) 'Build aircd first or pass -AircdExe.'
    New-Item -ItemType Directory -Path (Join-Path $TestRoot 'scripts'), (Join-Path $TestRoot 'isolated-config'), (Join-Path $TestRoot 'target\release'), (Join-Path $TestRoot 'target\debug') -Force | Out-Null
    $env:AIRCD_CONFIG_DIR = Join-Path $TestRoot 'isolated-config'
    $env:AIRCD_DATA_DIR = Join-Path $TestRoot 'isolated-data'
    Copy-Item -Path (Join-Path $PSScriptRoot '*.ps1'), (Join-Path $PSScriptRoot '*.psm1') -Destination (Join-Path $TestRoot 'scripts')
    $Release = Join-Path $TestRoot 'target\release\aircd.exe'
    $Debug = Join-Path $TestRoot 'target\debug\aircd.exe'
    Copy-Item -LiteralPath $AircdExe -Destination $Release
    $IrcPort = Get-FreePort
    do { $HttpPort = Get-FreePort } while ($HttpPort -eq $IrcPort)
    $Db = Join-Path $TestRoot 'data folder\hub.sqlite3'
    $Config = New-AircdTrayConfig -RepoRoot $TestRoot -Bind "127.0.0.1:$IrcPort" -HttpBind "127.0.0.1:$HttpPort" -DbPath $Db -Profile 'profile with spaces'
    $OriginalArguments = @($Config.ServerArguments) + @('--open-registration')
    $Config.ServerArguments = $OriginalArguments

    $Values = @('fixture.exe', '', 'ordinary', 'with spaces', 'literal"quote', 'C:\trailing slash\', 'slash\"quote', '$dollar`tick')
    $Parsed = [AircdTray.Native]::Split((Join-AircdArguments $Values))
    Assert (($Parsed | ConvertTo-Json -Compress) -ceq ($Values | ConvertTo-Json -Compress)) 'Windows argument quoting roundtrip failed.'
    Assert-Throws { New-AircdTrayConfig -RepoRoot $TestRoot -Bind 'remote:6667' } 'local numeric'
    Assert-Throws { New-AircdTrayConfig -RepoRoot $TestRoot -Bind '127.0.0.1:6680' } 'different ports'
    Assert ((Get-AircdTrayMutexName $Config) -eq (Get-AircdTrayMutexName $Config)) 'Mutex identity changed.'
    Assert ((Invoke-AircdTrayAction $Config 'Status').State -eq 'Stopped') 'Initial status should be stopped.'
    Assert-Throws { Invoke-AircdTrayAction $Config 'Tui' } 'before launching'

    # Editable config file + env + CLI precedence for tray config.
    $TrayCfgFile = Join-Path $TestRoot 'tray-config.json'
    @{
        server = @{ bind = "127.0.0.1:$IrcPort"; http_bind = "127.0.0.1:$HttpPort"; db_path = 'tray-data\tray.sqlite3'; open_registration = $true }
        tray   = @{ profile = 'cfg-profile' }
    } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $TrayCfgFile -Encoding UTF8
    $TrayFromFile = New-AircdTrayConfig -RepoRoot $TestRoot -ConfigPath $TrayCfgFile
    Assert ($TrayFromFile.Profile -eq 'cfg-profile' -and $TrayFromFile.DbPath -eq [IO.Path]::GetFullPath((Join-Path $TestRoot 'tray-data\tray.sqlite3')) -and ($TrayFromFile.ServerArguments -contains '--open-registration')) 'Tray config file was not loaded.'
    $env:AIRCD_PROFILE = 'env-profile'
    $TrayFromEnv = New-AircdTrayConfig -RepoRoot $TestRoot -ConfigPath $TrayCfgFile
    Assert ($TrayFromEnv.Profile -eq 'env-profile') 'Tray config did not honor Env > ConfigFile.'
    $TrayFromCli = New-AircdTrayConfig -RepoRoot $TestRoot -ConfigPath $TrayCfgFile -Profile 'cli-profile'
    Assert ($TrayFromCli.Profile -eq 'cli-profile') 'Tray config did not honor CLI > Env.'
    Remove-Item Env:\AIRCD_PROFILE -ErrorAction SilentlyContinue

    # Run real menu Click handlers against a real hub on private ports/database.
    $script:MenuResult = $null
    $script:Clicked = @()
    $Menu = New-AircdTrayMenu -OnAction {
        param($Action)
        $script:Clicked += $Action
        if ($Action -in @('Restart', 'Stop', 'Tui')) { $script:MenuResult = Invoke-AircdTrayAction $Config $Action }
    }
    Assert ($Menu.Items[2].Text -eq 'Restart Server' -and $Menu.Items[3].Text -eq 'Launch TUI' -and
        $Menu.Items[4].Text -eq 'Stop Server') 'Unexpected menu commands.'
    $Menu.Items[2].PerformClick()
    Assert ($script:MenuResult.State -eq 'Running') 'Restart while stopped should start the server.'
    $First = $script:MenuResult.Server
    Assert ($First.Executable -ieq $Release) 'Initial launch did not choose release.'
    $Same = Invoke-AircdTrayAction $Config 'Start'
    Assert ($Same.Server.Id -eq $First.Id) 'Start should adopt the existing hub.'

    # Persist a marker via the real API and ensure it survives full replacement.
    $Registration = Invoke-RestMethod -UseBasicParsing -Uri "$($Config.Url)/v1/register" -Method Post -ContentType 'application/json' -Body '{"nick":"tray-test","role":"worker"}'
    Assert ($Registration.token) 'Registration fixture did not persist.'
    Copy-Item -LiteralPath $AircdExe -Destination $Debug
    (Get-Item -LiteralPath $Debug).LastWriteTimeUtc = [datetime]::UtcNow.AddMinutes(1)
    $Menu.Items[2].PerformClick()
    $Second = $script:MenuResult.Server
    Assert ($Second.Id -ne $First.Id -and $First.Process.HasExited) 'Old server did not fully exit before replacement.'
    Assert ($Second.Executable -ieq $Debug) 'Restart did not select the newer debug build.'
    Assert (($Second.Arguments | ConvertTo-Json -Compress) -ceq ($OriginalArguments | ConvertTo-Json -Compress)) 'Restart lost server flags.'
    $Agents = Invoke-RestMethod -UseBasicParsing -Uri "$($Config.Url)/v1/agents"
    Assert (($Agents | ConvertTo-Json -Depth 8) -match 'tray-test') 'Database state was lost across restart.'

    # A matching executable on the port with a different DB must be rejected.
    $OtherConfig = New-AircdTrayConfig -RepoRoot $TestRoot -Bind $Config.Bind -HttpBind $Config.HttpBind -DbPath (Join-Path $TestRoot 'other.sqlite3')
    Assert-Throws { Invoke-AircdTrayAction $OtherConfig 'Restart' } 'different bind/database'
    Assert (!$Second.Process.HasExited) 'Mismatched controller killed the server.'

    # A missing replacement build must leave the current healthy server up.
    $MissingConfig = New-AircdTrayConfig -RepoRoot (Join-Path $TestRoot 'missing') -Bind $Config.Bind -HttpBind $Config.HttpBind -DbPath $Config.DbPath
    New-Item -ItemType Directory -Path (Join-Path $MissingConfig.RepoRoot 'scripts') -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'Get-AircdExecutable.ps1') -Destination (Join-Path $MissingConfig.RepoRoot 'scripts')
    Assert-Throws { Invoke-AircdTrayAction $MissingConfig 'Restart' } 'No aircd build'
    Assert (!$Second.Process.HasExited) 'Missing build killed the healthy server.'

    # Substitute a real console executable solely for the TUI launch. The hub
    # stays on its original debug image; the new release captures exact argv.
    $Capture = Join-Path $TestRoot 'tui arguments.json'
    $Fixture = Join-Path $TestRoot 'fixture.exe'
    Add-Type -OutputAssembly $Fixture -OutputType ConsoleApplication -TypeDefinition @'
using System;
using System.IO;
public static class TrayTuiFixture {
    public static int Main(string[] args) {
        if (args.Length == 0 || args[0] != "tui") return 17;
        File.WriteAllLines(Environment.GetEnvironmentVariable("AIRCD_TRAY_TEST_CAPTURE"), args);
        System.Threading.Thread.Sleep(15000);
        return 0;
    }
}
'@
    Copy-Item -LiteralPath $Fixture -Destination $Release -Force
    (Get-Item -LiteralPath $Release).LastWriteTimeUtc = [datetime]::UtcNow.AddMinutes(2)
    $env:AIRCD_TRAY_TEST_CAPTURE = $Capture
    $Menu.Items[3].PerformClick()
    $CaptureDeadline = [datetime]::UtcNow.AddSeconds(5)
    while (!(Test-Path -LiteralPath $Capture) -and [datetime]::UtcNow -lt $CaptureDeadline) { Start-Sleep -Milliseconds 100 }
    Assert (Test-Path -LiteralPath $Capture) 'TUI fixture did not start within five seconds.'
    Assert ((Get-Content -LiteralPath $Capture) -join '|' -eq "tui|--server|$($Config.Url)|--profile|profile with spaces") 'TUI menu did not launch newest binary with correct arguments.'
    $TuiInfo = Get-CimInstance Win32_Process -Filter "Name = 'aircd.exe'" | Where-Object { $_.ExecutablePath -ieq $Release }
    $Tui = Get-Process -Id $TuiInfo.ProcessId
    $Menu.Items[4].PerformClick()
    Assert ($script:MenuResult.State -eq 'Stopped' -and $Second.Process.HasExited) 'Stop menu failed.'
    Assert (!$Tui.HasExited) 'Stop Server terminated the TUI.'
    Assert (@(Get-AircdListeners $Config).Count -eq 0) 'Stop left server ports bound.'
    foreach ($Item in $Menu.Items) { if ($Item.Tag -in @('Logs', 'Details', 'Exit')) { $Item.PerformClick() } }
    Assert (($script:Clicked -join '|') -eq 'Restart|Restart|Tui|Stop|Logs|Details|Exit') 'Menu actions were routed incorrectly.'
    $Menu.Dispose()

    # Bad binary/readiness failure must leave no server or partial listeners.
    Assert-Throws { Invoke-AircdTrayAction $Config 'Start' } 'Server exited with code'
    Assert (@(Get-AircdListeners $Config).Count -eq 0) 'Failed start left listeners.'

    # Unknown listener must survive a stop/restart attempt.
    $Unrelated = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, $Config.HttpPort)
    $Unrelated.Start()
    Assert-Throws { Invoke-AircdTrayAction $Config 'Restart' } 'unrecognized process'
    Assert (@(Get-AircdListeners $Config).Count -eq 1) 'Unrelated listener was stopped.'
    $Unrelated.Stop()
    $Unrelated = $null
    if (!$Tui.HasExited) { $Tui.Kill(); $null = $Tui.WaitForExit(10000) }
    Copy-Item -LiteralPath $AircdExe -Destination $Release -Force
    (Get-Item -LiteralPath $Release).LastWriteTimeUtc = [datetime]::UtcNow.AddMinutes(3)

    # Installer creates a real .lnk under a private Startup fixture. It must
    # migrate only our legacy entry, preserve a backup, and uninstall cleanly.
    $Startup = Join-Path $TestRoot 'startup folder'
    New-Item -ItemType Directory -Path $Startup -Force | Out-Null
    $Wsh = New-Object -ComObject WScript.Shell
    $LegacyPath = Join-Path $Startup 'CAMS hub.lnk'
    $Legacy = $Wsh.CreateShortcut($LegacyPath)
    $Legacy.TargetPath = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
    $Legacy.Arguments = Join-AircdArguments @('-File', (Join-Path $TestRoot 'scripts\Start-Aircd.ps1'), '-Background', '-IfNotRunning')
    $Legacy.Save()
    $Installer = Join-Path $TestRoot 'scripts\Install-AircdTray.ps1'
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Installer -StartupDirectory $Startup -Bind $Config.Bind -HttpBind $Config.HttpBind -DbPath $Config.DbPath -Profile 'profile with spaces' -StartNow
    Assert ($LASTEXITCODE -eq 0) 'Installer failed.'
    $ShortcutPath = Join-Path $Startup 'CAMS tray.lnk'
    Assert ((Test-Path -LiteralPath $ShortcutPath) -and !(Test-Path -LiteralPath $LegacyPath) -and (Test-Path -LiteralPath "$LegacyPath.before-tray")) 'Legacy migration failed.'
    $Shortcut = $Wsh.CreateShortcut($ShortcutPath)
    $Arguments = [AircdTray.Native]::Split(('powershell.exe ' + $Shortcut.Arguments))
    Assert ($Arguments -contains (Join-Path $TestRoot 'scripts\Start-AircdTray.ps1')) 'Shortcut lost script path with spaces.'
    Assert ($Arguments -contains $Config.DbPath -and $Arguments -contains 'profile with spaces' -and $Arguments -contains '-STA') 'Shortcut lost settings.'
    # Exercise the actual message loop and worker runspace, not just the core
    # functions. The display log is written when NotifyIcon state is assigned.
    Wait-TrayLog 'Tray Running:'
    $UiStatus = Invoke-AircdTrayAction $Config 'Status'
    Assert ($UiStatus.State -eq 'Running') 'Installed tray did not start the real hub.'
    Assert (@(Get-TestTrays).Count -eq 1) 'Expected exactly one tray.'
    $Duplicate = Start-Process -FilePath $Shortcut.TargetPath -ArgumentList $Shortcut.Arguments -WindowStyle Hidden -PassThru
    $null = $Duplicate.Handle
    Assert ($Duplicate.WaitForExit(10000)) 'Duplicate tray did not exit.'
    Assert ($Duplicate.ExitCode -eq 0 -and @(Get-TestTrays).Count -eq 1) 'Duplicate tray was not suppressed.'
    $null = Invoke-AircdTrayAction $Config 'Stop'
    Wait-TrayLog 'Tray Stopped:'
    $null = Invoke-AircdTrayAction $Config 'Restart'
    $Restarted = Invoke-AircdTrayAction $Config 'Status'
    Assert ($Restarted.State -eq 'Running') 'Hub did not restart during UI status polling.'
    Wait-TrayLog "Tray Running: Running / idle - PID $($Restarted.Server.Id)"
    foreach ($TrayInfo in @(Get-TestTrays)) { Stop-Process -Id $TrayInfo.ProcessId -Force }
    Assert ((Invoke-AircdTrayAction $Config 'Status').Server.Id -eq $Restarted.Server.Id) 'Exiting tray stopped its hub.'
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Installer -StartupDirectory $Startup -Uninstall
    Assert ($LASTEXITCODE -eq 0 -and !(Test-Path -LiteralPath $ShortcutPath) -and (Test-Path -LiteralPath "$LegacyPath.before-tray")) 'Uninstall failed.'
} finally {
    foreach ($EnvName in $SavedEnv.Keys) {
        if ($null -eq $SavedEnv[$EnvName]) {
            Remove-Item -LiteralPath "Env:\$EnvName" -ErrorAction SilentlyContinue
        } else {
            Set-Item -LiteralPath "Env:\$EnvName" -Value $SavedEnv[$EnvName]
        }
    }
    if ($Unrelated) { $Unrelated.Stop() }
    foreach ($TrayInfo in @(Get-TestTrays)) {
        $CleanupProcess = Get-Process -Id $TrayInfo.ProcessId -ErrorAction SilentlyContinue
        if ($CleanupProcess) { $CleanupProcess.Kill(); $null = $CleanupProcess.WaitForExit(10000); $CleanupProcess.Dispose() }
    }
    if ($Config) {
        # Test root is private; fixture binary may no longer be an aircd server.
        Get-CimInstance Win32_Process -Filter "Name = 'aircd.exe'" | Where-Object {
            $_.ExecutablePath -and $_.ExecutablePath.StartsWith($TestRoot + '\', [StringComparison]::OrdinalIgnoreCase)
        } | ForEach-Object {
            $CleanupProcess = Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue
            if ($CleanupProcess) { $CleanupProcess.Kill(); $null = $CleanupProcess.WaitForExit(10000); $CleanupProcess.Dispose() }
        }
    }
    Remove-Item Env:\AIRCD_TRAY_TEST_CAPTURE -ErrorAction SilentlyContinue
    $ResolvedTestRoot = [IO.Path]::GetFullPath($TestRoot)
    if (!$ResolvedTestRoot.StartsWith($TempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        (Split-Path -Leaf $ResolvedTestRoot) -notlike 'aircd tray test *') { throw 'Refusing unexpected test cleanup path.' }
    if (Test-Path -LiteralPath $ResolvedTestRoot) { Remove-Item -LiteralPath $ResolvedTestRoot -Recurse -Force }
}
Write-Host 'PASS: real hub lifecycle/menu actions, newest build, old PID exit, DB/flags preservation, TUI argv/survival, listener protection, failed start cleanup, startup migration/uninstall, actual tray worker/status transitions, duplicate instance suppression, paths with spaces and quoting.'
