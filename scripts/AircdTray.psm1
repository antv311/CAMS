Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ScriptConfigModule = Join-Path $PSScriptRoot 'AircdConfig.psm1'
if (Test-Path -LiteralPath $ScriptConfigModule -PathType Leaf) {
    Import-Module $ScriptConfigModule -Force
}

# Start-Process joins its ArgumentList on Windows PowerShell 5.1. Quote using
# the Windows argv rules, including quotes and trailing backslashes.
function Join-AircdArguments([string[]]$Values) {
    ($Values | ForEach-Object {
        '"' + [regex]::Replace([regex]::Replace($_, '(\\*)"', '$1$1\"'), '(\\+)$', '$1$1') + '"'
    }) -join ' '
}

function Initialize-AircdNative {
    if ('AircdTray.Native' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace AircdTray {
    public static class Native {
        [DllImport("shell32.dll", SetLastError=true)]
        static extern IntPtr CommandLineToArgvW([MarshalAs(UnmanagedType.LPWStr)] string command, out int count);
        [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr memory);
        [DllImport("user32.dll")] public static extern bool DestroyIcon(IntPtr icon);
        public static string[] Split(string command) {
            int count;
            IntPtr argv = CommandLineToArgvW(command, out count);
            if (argv == IntPtr.Zero) throw new System.ComponentModel.Win32Exception();
            try {
                string[] result = new string[count];
                for (int i = 0; i < count; i++) result[i] = Marshal.PtrToStringUni(Marshal.ReadIntPtr(argv, i * IntPtr.Size));
                return result;
            } finally { LocalFree(argv); }
        }
    }
}
'@
}

function New-AircdTrayConfig {
    param(
        [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot),
        [string]$ConfigPath = '',
        [string]$Bind = '127.0.0.1:6667',
        [string]$HttpBind = '127.0.0.1:6680',
        [string]$DbPath = '',
        [string]$Profile = '',
        [switch]$OpenRegistration,
        [string]$RegisterKey = '',
        [string]$TlsCert = '',
        [string]$TlsKey = '',
        [string]$TlsBind = '127.0.0.1:6697',
        [string]$ExecutablePath = '',
        [string]$StartupDirectory = ''
    )

    $Overrides = @{}
    foreach ($Key in @('Bind', 'HttpBind', 'DbPath', 'Profile', 'OpenRegistration', 'RegisterKey', 'TlsCert', 'TlsKey', 'TlsBind', 'ExecutablePath', 'StartupDirectory')) {
        if ($PSBoundParameters.ContainsKey($Key)) {
            $Overrides[$Key] = $PSBoundParameters[$Key]
        }
    }

    $ResolvedCfg = Resolve-AircdWindowsConfig -RepoRoot $RepoRoot -ConfigPath $ConfigPath -Overrides $Overrides
    $Bind = $ResolvedCfg.Bind
    $HttpBind = $ResolvedCfg.HttpBind
    $DbPath = $ResolvedCfg.DbPath
    $Profile = $ResolvedCfg.Profile
    $OpenRegistration = [bool]$ResolvedCfg.OpenRegistration
    $RegisterKey = $ResolvedCfg.RegisterKey
    $TlsCert = $ResolvedCfg.TlsCert
    $TlsKey = $ResolvedCfg.TlsKey
    $TlsBind = $ResolvedCfg.TlsBind
    $ExecutablePath = $ResolvedCfg.ExecutablePath

    foreach ($Address in @($Bind, $HttpBind)) {
        if ($Address -notmatch '^(127\.0\.0\.1|0\.0\.0\.0|\[::1\]|\[::\]):([0-9]+)$' -or
            [int]$Matches[2] -lt 1 -or [int]$Matches[2] -gt 65535) {
            throw "Use a local numeric bind address and port: $Address"
        }
    }
    $HttpPort = [int]($HttpBind -split ':')[-1]
    $IrcPort = [int]($Bind -split ':')[-1]
    if ($HttpPort -eq $IrcPort) { throw 'HTTP and IRC must use different ports.' }
    if ($DbPath -eq ':memory:') { throw 'Tray control requires a persistent database.' }
    $DbPath = [IO.Path]::GetFullPath($DbPath)

    $ServerArgs = @('--bind', $Bind, '--http-bind', $HttpBind, '--db', $DbPath)
    if ($OpenRegistration) { $ServerArgs += '--open-registration' }
    if (-not [string]::IsNullOrWhiteSpace($RegisterKey)) { $ServerArgs += @('--register-key', $RegisterKey) }

    $HasTlsCert = -not [string]::IsNullOrWhiteSpace($TlsCert)
    $HasTlsKey = -not [string]::IsNullOrWhiteSpace($TlsKey)
    if ($HasTlsCert -ne $HasTlsKey) {
        throw 'Both -TlsCert and -TlsKey must be specified together to enable TLS.'
    }
    if ($HasTlsCert) {
        $FullTlsCert = [IO.Path]::GetFullPath($TlsCert)
        $FullTlsKey = [IO.Path]::GetFullPath($TlsKey)
        if (-not (Test-Path -LiteralPath $FullTlsCert -PathType Leaf)) { throw "TLS certificate not found: $FullTlsCert" }
        if (-not (Test-Path -LiteralPath $FullTlsKey -PathType Leaf)) { throw "TLS private key not found: $FullTlsKey" }
        if ($TlsBind -notmatch '^(127\.0\.0\.1|0\.0\.0\.0|\[::1\]|\[::\]):([0-9]+)$' -or
            [int]$Matches[2] -lt 1 -or [int]$Matches[2] -gt 65535) {
            throw "Use a local numeric TLS bind address and port: $TlsBind"
        }
        $ServerArgs += @('--tls-cert', $FullTlsCert, '--tls-key', $FullTlsKey, '--tls-bind', $TlsBind)
    }

    $ResolvedExe = ''
    if (-not [string]::IsNullOrWhiteSpace($ExecutablePath)) {
        $ResolvedExe = [IO.Path]::GetFullPath($ExecutablePath)
    }

    $UrlHost = if ($HttpBind.StartsWith('[')) { '[::1]' } else { '127.0.0.1' }
    [pscustomobject]@{
        RepoRoot = $ResolvedCfg.RepoRoot; ConfigPath = $ResolvedCfg.ConfigPath
        Bind = $Bind; HttpBind = $HttpBind
        DbPath = $DbPath; Profile = $Profile; HttpPort = $HttpPort; IrcPort = $IrcPort
        Url = "http://${UrlHost}:$HttpPort"; LogDir = (Split-Path -Parent $DbPath)
        ExecutablePath = $ResolvedExe; StartupDirectory = $ResolvedCfg.StartupDirectory
        ServerArguments = $ServerArgs
    }
}

function Resolve-AircdTrayExecutable($Config) {
    if ($Config.PSObject.Properties['ExecutablePath'] -and -not [string]::IsNullOrWhiteSpace($Config.ExecutablePath)) {
        if (-not (Test-Path -LiteralPath $Config.ExecutablePath -PathType Leaf)) {
            throw "Configured aircd executable not found: $($Config.ExecutablePath)"
        }
        return $Config.ExecutablePath
    }
    & (Join-Path $Config.RepoRoot 'scripts\Get-AircdExecutable.ps1') -RepoRoot $Config.RepoRoot
}

function Get-AircdTrayMutexName($Config) {
    $Hasher = [Security.Cryptography.SHA256]::Create()
    try {
        $Hash = [BitConverter]::ToString($Hasher.ComputeHash([Text.Encoding]::UTF8.GetBytes(
            "$($Config.IrcPort)|$($Config.HttpPort)"))).Replace('-', '')
        "Local\CAMS.Tray.$Hash"
    } finally { $Hasher.Dispose() }
}

function Get-AircdListeners($Config) {
    # Conservative: any owner of either configured port must be identified.
    @(Get-NetTCPConnection -State Listen -ErrorAction Stop | Where-Object {
        $_.LocalPort -eq $Config.HttpPort -or $_.LocalPort -eq $Config.IrcPort
    })
}

function Get-AircdServer($Config) {
    $Listeners = @(Get-AircdListeners $Config)
    if (!$Listeners.Count) { return $null }
    $Owners = @($Listeners.OwningProcess | Sort-Object -Unique)
    if ($Owners.Count -ne 1) { throw 'Configured ports belong to different processes; no process was stopped.' }
    $OwnerId = [int]$Owners[0]
    $Info = Get-CimInstance Win32_Process -Filter "ProcessId = $OwnerId" -ErrorAction Stop
    if (!$Info -or !$Info.ExecutablePath -or !$Info.CommandLine -or
        [IO.Path]::GetFileName($Info.ExecutablePath) -ine 'aircd.exe') {
        throw "Port belongs to an unrecognized process (PID $OwnerId); no process was stopped."
    }
    Initialize-AircdNative
    $Argv = [AircdTray.Native]::Split($Info.CommandLine)
    $ArgsOnly = @($Argv | Select-Object -Skip 1)
    if ($ArgsOnly.Count -and $ArgsOnly[0] -eq 'tui') { throw 'Refusing to control a TUI process.' }
    $Settings = @{ '--bind' = '127.0.0.1:6667'; '--http-bind' = '127.0.0.1:6680'; '--db' = '' }
    for ($Index = 0; $Index -lt $ArgsOnly.Count; $Index++) {
        if ($Settings.ContainsKey($ArgsOnly[$Index]) -and $Index + 1 -lt $ArgsOnly.Count) {
            $Settings[$ArgsOnly[$Index]] = $ArgsOnly[$Index + 1]
            $Index++
        }
    }
    if ($Settings['--bind'] -ne $Config.Bind -or $Settings['--http-bind'] -ne $Config.HttpBind -or
        ![IO.Path]::IsPathRooted($Settings['--db']) -or
        [IO.Path]::GetFullPath($Settings['--db']) -ine $Config.DbPath) {
        throw 'Listening aircd uses different bind/database settings. Start the tray with matching -Bind, -HttpBind and absolute -DbPath; no process was stopped.'
    }
    $Process = Get-Process -Id $OwnerId -ErrorAction Stop
    $null = $Process.Handle
    if ($Process.HasExited -or $Process.Path -ine $Info.ExecutablePath -or
        [Math]::Abs(($Process.StartTime.ToUniversalTime() - $Info.CreationDate.ToUniversalTime()).TotalMilliseconds) -gt 1) {
        $Process.Dispose()
        throw 'Server identity changed during inspection; retry. No process was stopped.'
    }
    [pscustomobject]@{
        Process = $Process; Id = $OwnerId; Executable = $Info.ExecutablePath
        Arguments = $ArgsOnly; Started = $Process.StartTime
        BothPorts = (@($Listeners.LocalPort | Sort-Object -Unique).Count -eq 2)
    }
}

function Get-AircdTrayStatus($Config) {
    $Server = Get-AircdServer $Config
    if (!$Server) { return [pscustomobject]@{ State = 'Stopped'; Detail = 'Server stopped'; Server = $null } }
    try {
        $Health = Invoke-RestMethod -UseBasicParsing -Uri "$($Config.Url)/healthz" -TimeoutSec 2
        if (!$Server.BothPorts -or !$Health.ok -or $Health.service -ne 'aircd' -or
            $Health.http_port -ne $Config.HttpPort -or $Health.irc_port -ne $Config.IrcPort) {
            throw 'Server is not listening on both configured ports.'
        }
        [pscustomobject]@{ State = 'Running'; Detail = "Running / idle - PID $($Server.Id)"; Server = $Server }
    } catch {
        [pscustomobject]@{ State = 'Error'; Detail = "Server PID $($Server.Id) is not ready"; Server = $Server }
    }
}

function Write-AircdTrayLog($Config, [string]$Message) {
    New-Item -ItemType Directory -Path $Config.LogDir -Force | Out-Null
    Add-Content -LiteralPath (Join-Path $Config.LogDir 'aircd.tray.log') -Value "$(Get-Date -Format o) $Message"
}

function Stop-AircdTrayServer($Config) {
    $Server = Get-AircdServer $Config
    if ($Server) {
        # Capture the OS process handle before stopping, so a reused PID cannot
        # redirect termination. Do not enumerate/kill by executable name.
        $null = $Server.Process.Handle
        if ($Server.Process.HasExited) { throw 'Server changed during stop; retry.' }
        $Config.ServerArguments = $Server.Arguments
        $Server.Process.Kill()
        if (!$Server.Process.WaitForExit(10000)) { throw "Server PID $($Server.Id) did not exit; replacement was not launched." }
        Write-AircdTrayLog $Config "Stopped server PID $($Server.Id)"
    }
    $Deadline = [datetime]::UtcNow.AddSeconds(10)
    while (@(Get-AircdListeners $Config).Count) {
        if ([datetime]::UtcNow -ge $Deadline) { throw 'Server ports remain occupied; replacement was not launched.' }
        Start-Sleep -Milliseconds 150
    }
}

function Start-AircdTrayServer($Config) {
    $Existing = Get-AircdServer $Config
    if ($Existing) {
        $Config.ServerArguments = $Existing.Arguments
        return Get-AircdTrayStatus $Config
    }
    $Exe = Resolve-AircdTrayExecutable $Config
    if (!$Exe) { throw 'No aircd build found. Run cargo build, then Restart Server.' }
    New-Item -ItemType Directory -Path $Config.LogDir -Force | Out-Null
    $Process = Start-Process -FilePath $Exe -ArgumentList (Join-AircdArguments $Config.ServerArguments) `
        -WorkingDirectory $Config.RepoRoot -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput (Join-Path $Config.LogDir 'aircd.stdout.log') `
        -RedirectStandardError (Join-Path $Config.LogDir 'aircd.stderr.log')
    Write-AircdTrayLog $Config "Starting server PID $($Process.Id) from $Exe"
    try {
        $Deadline = [datetime]::UtcNow.AddSeconds(20)
        while ([datetime]::UtcNow -lt $Deadline) {
            if ($Process.HasExited) { throw "Server exited with code $($Process.ExitCode). See aircd.stderr.log." }
            $Status = Get-AircdTrayStatus $Config
            if ($Status.State -eq 'Running') {
                if ($Status.Server.Id -ne $Process.Id) { throw 'Another server took the ports; replacement was not accepted.' }
                Write-AircdTrayLog $Config "Ready PID $($Process.Id) from $Exe"
                return $Status
            }
            Start-Sleep -Milliseconds 200
        }
        throw 'Server readiness timed out. See aircd.stderr.log.'
    } catch {
        # Only clean up the process we just launched. Never leave an orphan
        # after a partial bind or failed health check.
        if (!$Process.HasExited) { $Process.Kill(); $null = $Process.WaitForExit(10000) }
        throw
    }
}

function Open-AircdTrayTui($Config) {
    $Status = Get-AircdTrayStatus $Config
    if ($Status.State -ne 'Running') { throw 'Start or restart the server before launching the TUI.' }
    $Exe = Resolve-AircdTrayExecutable $Config
    if (!$Exe) { throw 'No aircd build found. Run cargo build.' }
    $Arguments = @('tui', '--server', $Config.Url)
    if ($Config.Profile) { $Arguments += @('--profile', $Config.Profile) }
    # A visible console is intentional: this is the interactive TUI requested
    # by the user. Server and tray launches use hidden windows.
    Start-Process -FilePath $Exe -ArgumentList (Join-AircdArguments $Arguments) `
        -WorkingDirectory $Config.RepoRoot -WindowStyle Normal -PassThru
}

function Invoke-AircdTrayAction($Config, [string]$Action) {
    switch ($Action) {
        'Start' { Start-AircdTrayServer $Config }
        'Restart' {
            # Resolve before stopping so a missing build leaves a running hub up.
            $Exe = Resolve-AircdTrayExecutable $Config
            if (!$Exe) { throw 'No aircd build found. Run cargo build before restarting.' }
            Stop-AircdTrayServer $Config
            Start-AircdTrayServer $Config
        }
        'Stop' { Stop-AircdTrayServer $Config; Get-AircdTrayStatus $Config }
        'Tui' { $null = Open-AircdTrayTui $Config; Get-AircdTrayStatus $Config }
        'Status' { Get-AircdTrayStatus $Config }
        default { throw "Unknown tray action: $Action" }
    }
}

function New-AircdTrayMenu([scriptblock]$OnAction) {
    Add-Type -AssemblyName System.Windows.Forms
    $Menu = New-Object Windows.Forms.ContextMenuStrip
    $StatusItem = $Menu.Items.Add('CAMS - checking server...')
    $StatusItem.Enabled = $false
    $null = $Menu.Items.Add((New-Object Windows.Forms.ToolStripSeparator))
    foreach ($Entry in @(
        @('Restart Server', 'Restart'), @('Launch TUI', 'Tui'), @('Stop Server', 'Stop'),
        @('Open Logs', 'Logs'), @('Server Details', 'Details'), @('Exit Tray (keep server running)', 'Exit')
    )) {
        $Item = $Menu.Items.Add($Entry[0])
        $Item.Tag = $Entry[1]
        # Use sender.Tag: each handler has the same closure without capturing
        # the final loop iteration's action.
        $Item.add_Click({ param($Sender, $EventArgs) & $OnAction ([string]$Sender.Tag) }.GetNewClosure())
    }
    $Menu
}

Export-ModuleMember -Function *-Aircd*
