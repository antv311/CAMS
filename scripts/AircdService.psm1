Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ScriptConfigModule = Join-Path $PSScriptRoot 'AircdConfig.psm1'
if (Test-Path -LiteralPath $ScriptConfigModule -PathType Leaf) {
    Import-Module $ScriptConfigModule -Force
}

# Start-Process and Scheduled Task arguments follow Windows CommandLineToArgvW
# rules, including embedded quotes and trailing backslashes.
function Join-AircdServiceArguments {
    param([string[]]$Values)
    if ($null -eq $Values -or $Values.Count -eq 0) { return '' }
    ($Values | ForEach-Object {
        $Val = if ($null -eq $_) { '' } else { [string]$_ }
        '"' + [regex]::Replace([regex]::Replace($Val, '(\\*)"', '$1$1\"'), '(\\+)$', '$1$1') + '"'
    }) -join ' '
}

function Initialize-AircdServiceNative {
    if ('AircdService.Native' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace AircdService {
    public static class Native {
        [DllImport("shell32.dll", SetLastError=true)]
        static extern IntPtr CommandLineToArgvW([MarshalAs(UnmanagedType.LPWStr)] string command, out int count);
        [DllImport("kernel32.dll")]
        static extern IntPtr LocalFree(IntPtr memory);
        public static string[] Split(string command) {
            int count;
            IntPtr argv = CommandLineToArgvW(command, out count);
            if (argv == IntPtr.Zero) throw new System.ComponentModel.Win32Exception();
            try {
                string[] result = new string[count];
                for (int i = 0; i < count; i++) {
                    result[i] = Marshal.PtrToStringUni(Marshal.ReadIntPtr(argv, i * IntPtr.Size));
                }
                return result;
            } finally { LocalFree(argv); }
        }
    }
}
'@
}

function Test-AircdServiceIsElevated {
    $Identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $Principal = New-Object Security.Principal.WindowsPrincipal($Identity)
    $Principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Test-AircdNumericBind {
    param([string]$Address, [string]$Label)
    if ($Address -notmatch '^(127\.0\.0\.1|0\.0\.0\.0|\[::1\]|\[::\]):([0-9]+)$' -or
        [int]$Matches[2] -lt 1 -or [int]$Matches[2] -gt 65535) {
        throw "Use a local numeric $Label bind address and port: $Address"
    }
    [int]$Matches[2]
}

function New-AircdServiceConfig {
    param(
        [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot),
        [string]$ConfigPath = '',
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
        [string]$ExecutablePath = ''
    )

    $Overrides = @{}
    foreach ($Key in @('Name', 'TaskPath', 'Bind', 'HttpBind', 'DbPath', 'OpenRegistration', 'RegisterKey', 'TlsCert', 'TlsKey', 'TlsBind', 'ExecutablePath')) {
        if ($PSBoundParameters.ContainsKey($Key)) {
            $Overrides[$Key] = $PSBoundParameters[$Key]
        }
    }

    $ResolvedCfg = Resolve-AircdWindowsConfig -RepoRoot $RepoRoot -ConfigPath $ConfigPath -Overrides $Overrides
    $Name = $ResolvedCfg.Name
    $TaskPath = $ResolvedCfg.TaskPath
    $Bind = $ResolvedCfg.Bind
    $HttpBind = $ResolvedCfg.HttpBind
    $DbPath = $ResolvedCfg.DbPath
    $OpenRegistration = [bool]$ResolvedCfg.OpenRegistration
    $RegisterKey = $ResolvedCfg.RegisterKey
    $TlsCert = $ResolvedCfg.TlsCert
    $TlsKey = $ResolvedCfg.TlsKey
    $TlsBind = $ResolvedCfg.TlsBind
    $ExecutablePath = $ResolvedCfg.ExecutablePath

    if ([string]::IsNullOrWhiteSpace($Name) -or $Name -notmatch '^[A-Za-z0-9._-]{1,64}$') {
        throw "Invalid service/task name '$Name'. Use 1-64 characters from [A-Za-z0-9._-]."
    }
    if ([string]::IsNullOrWhiteSpace($TaskPath)) { $TaskPath = '\' }
    if (-not $TaskPath.StartsWith('\')) { $TaskPath = '\' + $TaskPath }
    if (-not $TaskPath.EndsWith('\')) { $TaskPath = $TaskPath + '\' }

    $IrcPort = Test-AircdNumericBind -Address $Bind -Label 'IRC'
    $HttpPort = Test-AircdNumericBind -Address $HttpBind -Label 'HTTP'
    if ($HttpPort -eq $IrcPort) { throw 'HTTP and IRC must use different ports.' }

    if ($DbPath -eq ':memory:') { throw 'Autonomous service installation requires a persistent database path.' }
    $FullDbPath = [IO.Path]::GetFullPath($DbPath)
    $FullRepoRoot = $ResolvedCfg.RepoRoot

    $ServerArgs = @('--bind', $Bind, '--http-bind', $HttpBind, '--db', $FullDbPath)
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
        $null = Test-AircdNumericBind -Address $TlsBind -Label 'TLS'
        $ServerArgs += @('--tls-cert', $FullTlsCert, '--tls-key', $FullTlsKey, '--tls-bind', $TlsBind)
    }

    $ResolvedExe = ''
    if (-not [string]::IsNullOrWhiteSpace($ExecutablePath)) {
        $ResolvedExe = [IO.Path]::GetFullPath($ExecutablePath)
    }

    $LogDir = Split-Path -Parent $FullDbPath
    $ServiceDir = Join-Path $LogDir 'service'
    $UrlHost = if ($HttpBind.StartsWith('[')) { '[::1]' } else { '127.0.0.1' }
    $EventId = Get-AircdServiceHash "$Name|$IrcPort|$HttpPort|$FullDbPath"

    [pscustomobject]@{
        Name            = $Name
        TaskPath        = $TaskPath
        RepoRoot        = $FullRepoRoot
        ConfigPath      = $ResolvedCfg.ConfigPath
        Mode            = $ResolvedCfg.Mode
        Trigger         = $ResolvedCfg.Trigger
        Bind            = $Bind
        HttpBind        = $HttpBind
        DbPath          = $FullDbPath
        HttpPort        = $HttpPort
        IrcPort         = $IrcPort
        Url             = "http://${UrlHost}:$HttpPort"
        LogDir          = $LogDir
        ServiceDir      = $ServiceDir
        HostExePath     = Join-Path $ServiceDir 'aircd-service-host.exe'
        HostHashPath    = Join-Path $ServiceDir 'aircd-service-host.sha256'
        MetadataPath    = Join-Path $ServiceDir "$Name.json"
        ExecutablePath  = $ResolvedExe
        ServerArguments = $ServerArgs
        EventId         = $EventId
    }
}

function Get-AircdServiceHash {
    param([string]$Value)
    $Hasher = [Security.Cryptography.SHA256]::Create()
    try {
        $Bytes = [Text.Encoding]::UTF8.GetBytes($Value.ToLowerInvariant())
        ([BitConverter]::ToString($Hasher.ComputeHash($Bytes))).Replace('-', '').Substring(0, 24)
    } finally {
        $Hasher.Dispose()
    }
}

function Get-AircdServiceStopEventName {
    param($Config, [bool]$CrossSession)
    $Prefix = if ($CrossSession) { 'Global' } else { 'Local' }
    "$Prefix\CAMS.Service.Stop.$($Config.EventId)"
}

function Resolve-AircdServiceExecutable {
    param($Config)
    if ($Config.ExecutablePath) {
        if (-not (Test-Path -LiteralPath $Config.ExecutablePath -PathType Leaf)) {
            throw "Configured aircd executable not found: $($Config.ExecutablePath)"
        }
        return $Config.ExecutablePath
    }
    $Resolver = Join-Path $Config.RepoRoot 'scripts\Get-AircdExecutable.ps1'
    if (Test-Path -LiteralPath $Resolver -PathType Leaf) {
        $Exe = & $Resolver -RepoRoot $Config.RepoRoot
        if ($Exe) { return $Exe }
    }
    throw 'No aircd build found. Run cargo build before installing or starting.'
}

function Get-AircdServiceHostSource {
    @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.ServiceProcess;
using System.Text;
using System.Threading;

namespace AircdServiceHost {
    public static class Program {
        public static int Main(string[] args) {
            bool isService = false;
            bool isRun = false;
            string serviceName = "CAMS-Aircd";
            string repoRoot = AppDomain.CurrentDomain.BaseDirectory;
            string logDir = AppDomain.CurrentDomain.BaseDirectory;
            string stopEventName = "";
            string explicitExe = "";
            List<string> serverArgs = new List<string>();

            for (int i = 0; i < args.Length; i++) {
                string arg = args[i];
                if (arg == "--") {
                    for (int j = i + 1; j < args.Length; j++) serverArgs.Add(args[j]);
                    break;
                } else if (arg == "--service" && i + 1 < args.Length) {
                    isService = true;
                    serviceName = args[++i];
                } else if (arg == "--run") {
                    isRun = true;
                } else if (arg == "--repo-root" && i + 1 < args.Length) {
                    repoRoot = args[++i];
                } else if (arg == "--log-dir" && i + 1 < args.Length) {
                    logDir = args[++i];
                } else if (arg == "--stop-event" && i + 1 < args.Length) {
                    stopEventName = args[++i];
                } else if (arg == "--exe" && i + 1 < args.Length) {
                    explicitExe = args[++i];
                }
            }

            if (isService) {
                ServiceBase.Run(new AircdWindowsService(serviceName, repoRoot, logDir, explicitExe, serverArgs));
                return 0;
            }
            if (isRun) {
                HostRunner runner = new HostRunner(repoRoot, logDir, stopEventName, explicitExe, serverArgs);
                return runner.RunStandalone();
            }
            return 2;
        }
    }

    public sealed class HostRunner {
        private readonly string _repoRoot;
        private readonly string _logDir;
        private readonly string _stopEventName;
        private readonly string _explicitExe;
        private readonly List<string> _serverArgs;
        private readonly object _logLock = new object();

        private Process _child;
        private StreamWriter _stdoutWriter;
        private StreamWriter _stderrWriter;

        public HostRunner(string repoRoot, string logDir, string stopEventName, string explicitExe, List<string> serverArgs) {
            _repoRoot = repoRoot;
            _logDir = logDir;
            _stopEventName = stopEventName;
            _explicitExe = explicitExe;
            _serverArgs = serverArgs ?? new List<string>();
        }

        public static string ResolveExecutable(string repoRoot, string explicitExe) {
            if (!string.IsNullOrEmpty(explicitExe)) {
                return File.Exists(explicitExe) ? Path.GetFullPath(explicitExe) : null;
            }
            string[] candidates = new string[] {
                Path.Combine(repoRoot, "target", "release", "aircd.exe"),
                Path.Combine(repoRoot, "target", "debug", "aircd.exe")
            };
            string bestPath = null;
            DateTime bestTime = DateTime.MinValue;
            foreach (string candidate in candidates) {
                if (File.Exists(candidate)) {
                    DateTime mtime = File.GetLastWriteTimeUtc(candidate);
                    if (bestPath == null || mtime > bestTime) {
                        bestPath = candidate;
                        bestTime = mtime;
                    }
                }
            }
            return bestPath;
        }

        public static string JoinWindowsArguments(IEnumerable<string> values) {
            List<string> parts = new List<string>();
            foreach (string raw in values) {
                string val = raw ?? "";
                StringBuilder sb = new StringBuilder();
                sb.Append('"');
                int backslashes = 0;
                for (int i = 0; i < val.Length; i++) {
                    char c = val[i];
                    if (c == '\\') {
                        backslashes++;
                    } else if (c == '"') {
                        sb.Append('\\', backslashes * 2 + 1);
                        sb.Append('"');
                        backslashes = 0;
                    } else {
                        sb.Append('\\', backslashes);
                        sb.Append(c);
                        backslashes = 0;
                    }
                }
                sb.Append('\\', backslashes * 2);
                sb.Append('"');
                parts.Add(sb.ToString());
            }
            return string.Join(" ", parts.ToArray());
        }

        public void WriteServiceLog(string message) {
            try {
                Directory.CreateDirectory(_logDir);
                string path = Path.Combine(_logDir, "aircd.service.log");
                lock (_logLock) {
                    using (FileStream fs = new FileStream(path, FileMode.Append, FileAccess.Write, FileShare.ReadWrite))
                    using (StreamWriter sw = new StreamWriter(fs, Encoding.UTF8)) {
                        sw.WriteLine(DateTime.UtcNow.ToString("o") + " " + message);
                    }
                }
            } catch { }
        }

        public Process StartChild() {
            Directory.CreateDirectory(_logDir);
            string exe = ResolveExecutable(_repoRoot, _explicitExe);
            if (string.IsNullOrEmpty(exe)) {
                throw new FileNotFoundException("No aircd.exe build found under " + _repoRoot);
            }

            _stdoutWriter = new StreamWriter(new FileStream(
                Path.Combine(_logDir, "aircd.stdout.log"), FileMode.Append, FileAccess.Write, FileShare.ReadWrite), Encoding.UTF8);
            _stdoutWriter.AutoFlush = true;
            _stderrWriter = new StreamWriter(new FileStream(
                Path.Combine(_logDir, "aircd.stderr.log"), FileMode.Append, FileAccess.Write, FileShare.ReadWrite), Encoding.UTF8);
            _stderrWriter.AutoFlush = true;

            string banner = "=== " + DateTime.UtcNow.ToString("o") + " Starting " + exe + " ===";
            lock (_logLock) {
                _stdoutWriter.WriteLine(banner);
                _stderrWriter.WriteLine(banner);
            }

            ProcessStartInfo psi = new ProcessStartInfo();
            psi.FileName = exe;
            psi.Arguments = JoinWindowsArguments(_serverArgs);
            psi.WorkingDirectory = Directory.Exists(_repoRoot) ? _repoRoot : _logDir;
            psi.UseShellExecute = false;
            psi.CreateNoWindow = true;
            psi.RedirectStandardOutput = true;
            psi.RedirectStandardError = true;

            Process child = new Process();
            child.StartInfo = psi;
            child.EnableRaisingEvents = true;
            child.OutputDataReceived += (sender, e) => {
                if (e.Data != null) {
                    lock (_logLock) {
                        if (_stdoutWriter != null) _stdoutWriter.WriteLine(e.Data);
                    }
                }
            };
            child.ErrorDataReceived += (sender, e) => {
                if (e.Data != null) {
                    lock (_logLock) {
                        if (_stderrWriter != null) _stderrWriter.WriteLine(e.Data);
                    }
                }
            };
            if (!child.Start()) {
                throw new InvalidOperationException("Failed to start " + exe);
            }
            _child = child;
            child.BeginOutputReadLine();
            child.BeginErrorReadLine();
            WriteServiceLog("Started aircd PID " + child.Id + " from " + exe);
            return child;
        }

        public void StopChild() {
            Process child = _child;
            if (child != null) {
                try {
                    if (!child.HasExited) {
                        WriteServiceLog("Stopping aircd PID " + child.Id);
                        child.Kill();
                        child.WaitForExit(10000);
                    }
                    if (child.HasExited) {
                        child.WaitForExit();
                    }
                } catch (Exception ex) {
                    WriteServiceLog("Error stopping aircd: " + ex.Message);
                }
            }
            CloseWriters();
        }

        public void CloseWriters() {
            lock (_logLock) {
                if (_stdoutWriter != null) {
                    try { _stdoutWriter.Dispose(); } catch { }
                    _stdoutWriter = null;
                }
                if (_stderrWriter != null) {
                    try { _stderrWriter.Dispose(); } catch { }
                    _stderrWriter = null;
                }
            }
        }

        public int RunStandalone() {
            EventWaitHandle stopEvent = null;
            ManualResetEvent childExited = new ManualResetEvent(false);
            try {
                if (!string.IsNullOrEmpty(_stopEventName)) {
                    stopEvent = new EventWaitHandle(false, EventResetMode.ManualReset, _stopEventName);
                    stopEvent.Reset();
                }
                Process child = StartChild();
                child.Exited += (sender, e) => childExited.Set();
                if (child.HasExited) childExited.Set();

                if (stopEvent != null) {
                    int signaled = WaitHandle.WaitAny(new WaitHandle[] { childExited, stopEvent });
                    if (signaled == 1) {
                        WriteServiceLog("Stop requested via event for PID " + child.Id);
                        StopChild();
                        return 0;
                    }
                } else {
                    childExited.WaitOne();
                }

                child.WaitForExit();
                int code = child.ExitCode;
                WriteServiceLog("aircd PID " + child.Id + " exited with code " + code);
                CloseWriters();
                return code;
            } catch (Exception ex) {
                WriteServiceLog("Host error: " + ex.Message);
                StopChild();
                return 1;
            } finally {
                childExited.Close();
                if (stopEvent != null) stopEvent.Close();
            }
        }
    }

    public sealed class AircdWindowsService : ServiceBase {
        private readonly HostRunner _runner;
        private int _stopping = 0;

        public AircdWindowsService(string serviceName, string repoRoot, string logDir, string explicitExe, List<string> serverArgs) {
            ServiceName = serviceName;
            CanStop = true;
            CanShutdown = true;
            _runner = new HostRunner(repoRoot, logDir, "", explicitExe, serverArgs);
        }

        protected override void OnStart(string[] args) {
            Interlocked.Exchange(ref _stopping, 0);
            Process child = _runner.StartChild();
            if (child.WaitForExit(300)) {
                int code = child.ExitCode != 0 ? child.ExitCode : 1;
                _runner.WriteServiceLog("aircd exited prematurely during startup with code " + code);
                _runner.CloseWriters();
                ExitCode = code;
                throw new InvalidOperationException("aircd exited during startup with code " + code);
            }
            child.Exited += (sender, e) => {
                if (Interlocked.CompareExchange(ref _stopping, 0, 0) == 0) {
                    int code = 1;
                    try { code = child.ExitCode != 0 ? child.ExitCode : 1; } catch { }
                    _runner.WriteServiceLog("Unexpected aircd exit with code " + code + "; exiting service host for recovery.");
                    _runner.CloseWriters();
                    ExitCode = code;
                    Environment.Exit(code);
                }
            };
            if (child.HasExited && Interlocked.CompareExchange(ref _stopping, 0, 0) == 0) {
                int code = child.ExitCode != 0 ? child.ExitCode : 1;
                _runner.WriteServiceLog("Unexpected aircd exit with code " + code);
                _runner.CloseWriters();
                ExitCode = code;
                Environment.Exit(code);
            }
        }

        protected override void OnStop() {
            Interlocked.Exchange(ref _stopping, 1);
            try { RequestAdditionalTime(10000); } catch { }
            _runner.StopChild();
            ExitCode = 0;
        }

        protected override void OnShutdown() {
            Interlocked.Exchange(ref _stopping, 1);
            _runner.StopChild();
            ExitCode = 0;
        }
    }
}
'@
}

function Initialize-AircdServiceHost {
    param($Config)
    New-Item -ItemType Directory -Path $Config.ServiceDir -Force | Out-Null
    $Source = Get-AircdServiceHostSource
    $ExpectedHash = Get-AircdServiceHash $Source
    $CurrentHash = if (Test-Path -LiteralPath $Config.HostHashPath -PathType Leaf) {
        (Get-Content -LiteralPath $Config.HostHashPath -Raw).Trim()
    } else { '' }

    if ((Test-Path -LiteralPath $Config.HostExePath -PathType Leaf) -and ($CurrentHash -eq $ExpectedHash)) {
        return $Config.HostExePath
    }

    # Ensure no active host from this ServiceDir locks the output binary.
    $RunningHosts = @(Get-CimInstance Win32_Process -Filter "Name = 'aircd-service-host.exe'" -ErrorAction SilentlyContinue | Where-Object {
        $_.ExecutablePath -and [string]::Equals($_.ExecutablePath, $Config.HostExePath, [StringComparison]::OrdinalIgnoreCase)
    })
    if ($RunningHosts.Count -gt 0) {
        throw "Cannot update '$($Config.HostExePath)' while it is running. Stop the service/task first."
    }

    # If the host binary was loaded into the current PowerShell process during a
    # test run, Add-Type cannot overwrite the locked file; normally it isn't.
    Add-Type -TypeDefinition $Source -OutputAssembly $Config.HostExePath -OutputType WindowsApplication -ReferencedAssemblies @('System.ServiceProcess.dll')
    Set-Content -LiteralPath $Config.HostHashPath -Value $ExpectedHash -NoNewline
    $Config.HostExePath
}

function Get-AircdServiceListeners {
    param($Config)
    @(Get-NetTCPConnection -State Listen -ErrorAction Stop | Where-Object {
        $_.LocalPort -eq $Config.HttpPort -or $_.LocalPort -eq $Config.IrcPort
    })
}

function Stop-AircdVerifiedProcess {
    param([System.Diagnostics.Process]$Process, [int]$ProcessId)
    if ($null -eq $Process) { return }
    try {
        if ($Process.HasExited) { return }
        $Process.Kill()
    } catch [System.InvalidOperationException] {
        # Process already exited between HasExited and Kill().
        return
    } catch [System.ComponentModel.Win32Exception] {
        # TerminateProcess returns Access Denied when the process is already terminating.
        if ($Process.WaitForExit(2000)) { return }
        throw
    }
    if (-not $Process.WaitForExit(10000)) {
        throw "aircd PID $ProcessId did not exit within 10 seconds."
    }
}

function Get-AircdServiceServer {
    param($Config)
    Initialize-AircdServiceNative
    $MaxAttempts = 20
    for ($Attempt = 1; $Attempt -le $MaxAttempts; $Attempt++) {
        $Listeners = @(Get-AircdServiceListeners $Config)
        if ($Listeners.Count -eq 0) { return $null }
        $Owners = @($Listeners.OwningProcess | Sort-Object -Unique)
        if ($Owners.Count -ne 1) {
            throw 'Configured ports belong to different processes; no process was stopped.'
        }
        $OwnerId = [int]$Owners[0]
        if ($OwnerId -le 0) {
            if ($Attempt -lt $MaxAttempts) { Start-Sleep -Milliseconds 50; continue }
            throw "Port belongs to an unrecognized process (PID $OwnerId); no process was stopped."
        }
        $Info = Get-CimInstance Win32_Process -Filter "ProcessId = $OwnerId" -ErrorAction SilentlyContinue
        if (-not $Info) {
            # Process exited after Get-NetTCPConnection while socket teardown is still completing.
            if ($Attempt -lt $MaxAttempts) { Start-Sleep -Milliseconds 50; continue }
            $ListenersAfter = @(Get-AircdServiceListeners $Config)
            if ($ListenersAfter.Count -eq 0) { return $null }
            throw "Port belongs to an exited process (PID $OwnerId); retry."
        }
        $ProcName = if ($Info.Name) { $Info.Name } else { '' }
        if ($ProcName -ine 'aircd.exe') {
            throw "Port belongs to an unrecognized process (PID $OwnerId); no process was stopped."
        }
        if (-not $Info.ExecutablePath -or -not $Info.CommandLine) {
            $Probe = Get-Process -Id $OwnerId -ErrorAction SilentlyContinue
            $ProbeExited = (-not $Probe) -or $Probe.HasExited
            if ($Probe) { $Probe.Dispose() }
            if ($ProbeExited -and $Attempt -lt $MaxAttempts) {
                Start-Sleep -Milliseconds 50
                continue
            }
            throw "Port belongs to aircd.exe (PID $OwnerId), but its command line cannot be inspected from this session. Run as Administrator or stop it manually."
        }
        $Argv = [AircdService.Native]::Split($Info.CommandLine)
        $ArgsOnly = @($Argv | Select-Object -Skip 1)
        if ($ArgsOnly.Count -gt 0 -and $ArgsOnly[0] -eq 'tui') {
            throw 'Refusing to control a TUI process.'
        }
        $Settings = @{ '--bind' = '127.0.0.1:6667'; '--http-bind' = '127.0.0.1:6680'; '--db' = '' }
        for ($Index = 0; $Index -lt $ArgsOnly.Count; $Index++) {
            if ($Settings.ContainsKey($ArgsOnly[$Index]) -and ($Index + 1) -lt $ArgsOnly.Count) {
                $Settings[$ArgsOnly[$Index]] = $ArgsOnly[$Index + 1]
                $Index++
            }
        }
        if ($Settings['--bind'] -ne $Config.Bind -or $Settings['--http-bind'] -ne $Config.HttpBind -or
            -not [IO.Path]::IsPathRooted($Settings['--db']) -or
            [IO.Path]::GetFullPath($Settings['--db']) -ine $Config.DbPath) {
            throw 'Listening aircd uses different bind/database settings. Pass matching -Bind, -HttpBind, and -DbPath; no process was stopped.'
        }

        $Process = Get-Process -Id $OwnerId -ErrorAction SilentlyContinue
        if (-not $Process) {
            if ($Attempt -lt $MaxAttempts) { Start-Sleep -Milliseconds 50; continue }
            $ListenersAfter = @(Get-AircdServiceListeners $Config)
            if ($ListenersAfter.Count -eq 0) { return $null }
            throw 'Server exited during inspection; retry. No process was stopped.'
        }
        $ExitedDuringInspect = $false
        $ProcPath = $null
        $ProcStartUtc = $null
        try {
            $null = $Process.Handle
            $ExitedDuringInspect = $Process.HasExited
            if (-not $ExitedDuringInspect) {
                $ProcPath = $Process.Path
                $ProcStartUtc = $Process.StartTime.ToUniversalTime()
                $ExitedDuringInspect = $Process.HasExited
            }
        } catch {
            if ($Process.HasExited) {
                $ExitedDuringInspect = $true
            } else {
                $Process.Dispose()
                throw
            }
        }
        if ($ExitedDuringInspect) {
            $Process.Dispose()
            if ($Attempt -lt $MaxAttempts) { Start-Sleep -Milliseconds 50; continue }
            $ListenersAfter = @(Get-AircdServiceListeners $Config)
            if ($ListenersAfter.Count -eq 0) { return $null }
            throw 'Server exited during inspection; retry. No process was stopped.'
        }
        if ($ProcPath -ine $Info.ExecutablePath -or
            [Math]::Abs(($ProcStartUtc - $Info.CreationDate.ToUniversalTime()).TotalMilliseconds) -gt 1) {
            $Process.Dispose()
            throw 'Server identity changed during inspection; retry. No process was stopped.'
        }
        return [pscustomobject]@{
            Process    = $Process
            Id         = $OwnerId
            Executable = $Info.ExecutablePath
            Arguments  = $ArgsOnly
            Started    = $Process.StartTime
            BothPorts  = (@($Listeners.LocalPort | Sort-Object -Unique).Count -eq 2)
        }
    }
}

function Test-AircdServiceHealth {
    param($Config)
    $Server = Get-AircdServiceServer $Config
    if (-not $Server) {
        return [pscustomobject]@{ State = 'Stopped'; Detail = 'Hub is not listening'; Server = $null }
    }
    try {
        $Health = Invoke-RestMethod -UseBasicParsing -Uri "$($Config.Url)/healthz" -TimeoutSec 2
        if (-not $Server.BothPorts -or -not $Health.ok -or $Health.service -ne 'aircd' -or
            $Health.http_port -ne $Config.HttpPort -or $Health.irc_port -ne $Config.IrcPort) {
            throw 'Server is not listening on both configured ports.'
        }
        [pscustomobject]@{ State = 'Running'; Detail = "Running - PID $($Server.Id)"; Server = $Server }
    } catch {
        [pscustomobject]@{ State = 'Error'; Detail = "Server PID $($Server.Id) is not ready"; Server = $Server }
    }
}

function Save-AircdServiceMetadata {
    param($Config, [string]$InstalledMode, [string]$StopEventName, [string]$TriggerKind, [string]$LogonType)
    New-Item -ItemType Directory -Path $Config.ServiceDir -Force | Out-Null
    $Meta = [ordered]@{
        name           = $Config.Name
        mode           = $InstalledMode
        task_path      = $Config.TaskPath
        trigger        = $TriggerKind
        logon_type     = $LogonType
        stop_event     = $StopEventName
        bind           = $Config.Bind
        http_bind      = $Config.HttpBind
        db_path        = $Config.DbPath
        repo_root      = $Config.RepoRoot
        executable     = $Config.ExecutablePath
        host_exe       = $Config.HostExePath
        installed_utc  = [datetime]::UtcNow.ToString('o')
    }
    Set-Content -LiteralPath $Config.MetadataPath -Value ($Meta | ConvertTo-Json -Depth 4) -Encoding UTF8
}

function Read-AircdServiceMetadata {
    param($Config)
    if (Test-Path -LiteralPath $Config.MetadataPath -PathType Leaf) {
        try {
            return (Get-Content -LiteralPath $Config.MetadataPath -Raw | ConvertFrom-Json)
        } catch {
            return $null
        }
    }
    $null
}

function Get-AircdServiceInstallation {
    param($Config, [string]$Mode = 'Auto')
    $ServiceObj = $null
    $TaskObj = $null

    if ($Mode -in @('Auto', 'Service')) {
        $ServiceObj = Get-Service -Name $Config.Name -ErrorAction SilentlyContinue
    }
    if ($Mode -in @('Auto', 'ScheduledTask')) {
        $TaskObj = Get-ScheduledTask -TaskName $Config.Name -TaskPath $Config.TaskPath -ErrorAction SilentlyContinue
    }

    $Meta = Read-AircdServiceMetadata $Config
    if ($ServiceObj -and ($Mode -eq 'Service' -or -not $TaskObj -or ($Meta -and $Meta.mode -eq 'Service'))) {
        return [pscustomobject]@{
            Installed = $true
            Mode      = 'Service'
            Service   = $ServiceObj
            Task      = $null
            Metadata  = $Meta
        }
    }
    if ($TaskObj) {
        return [pscustomobject]@{
            Installed = $true
            Mode      = 'ScheduledTask'
            Service   = $null
            Task      = $TaskObj
            Metadata  = $Meta
        }
    }
    [pscustomobject]@{
        Installed = $false
        Mode      = $null
        Service   = $null
        Task      = $null
        Metadata  = $Meta
    }
}

function Resolve-AircdServiceInstallPlan {
    param(
        [string]$Mode = 'Auto',
        [string]$Trigger = 'Auto',
        [switch]$RunAsSystem,
        [pscredential]$Credential,
        [switch]$SurviveLogout,
        [bool]$IsElevated = (Test-AircdServiceIsElevated)
    )

    if ($RunAsSystem -and $null -ne $Credential) {
        throw 'Specify either -RunAsSystem or -Credential, not both.'
    }

    $ResolvedMode = if ($Mode -eq 'Auto') { 'ScheduledTask' } else { $Mode }
    if ($ResolvedMode -eq 'Service') {
        if (-not $IsElevated) {
            throw 'Installing a Windows Service requires an elevated (Administrator) PowerShell prompt. Re-run as Administrator, or use -Mode ScheduledTask for a user logon scheduled task.'
        }
        return [pscustomobject]@{
            Mode          = 'Service'
            Trigger       = 'Automatic'
            LogonType     = if ($Credential) { 'Credential' } else { 'LocalSystem' }
            CrossSession  = $true
            SurvivesLogout = $true
            SurvivesReboot = $true
        }
    }

    $ResolvedTrigger = if ($Trigger -eq 'Auto') {
        if ($IsElevated) { 'AtStartup' } else { 'AtLogOn' }
    } else {
        $Trigger
    }

    $NeedsElevation = $RunAsSystem -or $SurviveLogout -or ($ResolvedTrigger -eq 'AtStartup')
    if ($NeedsElevation -and -not $IsElevated) {
        throw 'Configuring an AtStartup or logout-surviving scheduled task requires an elevated (Administrator) PowerShell prompt. Re-run as Administrator, or omit -SurviveLogout / use -Trigger AtLogOn for a current-user logon task.'
    }

    $LogonType = if ($RunAsSystem) {
        'ServiceAccount'
    } elseif ($null -ne $Credential) {
        'Password'
    } elseif ($IsElevated) {
        'S4U'
    } else {
        'Interactive'
    }

    $CrossSession = $LogonType -ne 'Interactive'
    [pscustomobject]@{
        Mode           = 'ScheduledTask'
        Trigger        = $ResolvedTrigger
        LogonType      = $LogonType
        CrossSession   = $CrossSession
        SurvivesLogout = $CrossSession
        SurvivesReboot = $true
    }
}

function Invoke-AircdServiceStopEvent {
    param($Config, $Installation)
    $Candidates = New-Object System.Collections.Generic.List[string]
    if ($Installation -and $Installation.Metadata -and $Installation.Metadata.stop_event) {
        $Candidates.Add([string]$Installation.Metadata.stop_event)
    }
    $Candidates.Add((Get-AircdServiceStopEventName -Config $Config -CrossSession $true))
    $Candidates.Add((Get-AircdServiceStopEventName -Config $Config -CrossSession $false))

    $Signaled = $false
    foreach ($EventName in ($Candidates | Select-Object -Unique)) {
        $Handle = $null
        try {
            if ([System.Threading.EventWaitHandle]::TryOpenExisting($EventName, [ref]$Handle)) {
                $null = $Handle.Set()
                $Signaled = $true
            }
        } catch {
            # Ignore inaccessible cross-session Global\ names when non-elevated.
        } finally {
            if ($null -ne $Handle) { $Handle.Dispose() }
        }
    }
    $Signaled
}

function Stop-AircdServiceInstance {
    param($Config, [string]$Mode = 'Auto')
    $Installation = Get-AircdServiceInstallation -Config $Config -Mode $Mode

    # Inspect port ownership first so an unrelated listener is never touched.
    $Server = Get-AircdServiceServer $Config
    try {
        if ($Installation.Installed) {
            if ($Installation.Mode -eq 'Service') {
                $Svc = Get-Service -Name $Config.Name -ErrorAction SilentlyContinue
                if ($Svc -and $Svc.Status -ne 'Stopped') {
                    Stop-Service -Name $Config.Name -Force -ErrorAction Stop
                    $Svc.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(15))
                }
            } elseif ($Installation.Mode -eq 'ScheduledTask') {
                $Signaled = Invoke-AircdServiceStopEvent -Config $Config -Installation $Installation
                if ($Signaled) {
                    $Deadline = [datetime]::UtcNow.AddSeconds(5)
                    while ([datetime]::UtcNow -lt $Deadline) {
                        $TaskNow = Get-ScheduledTask -TaskName $Config.Name -TaskPath $Config.TaskPath -ErrorAction SilentlyContinue
                        if (-not $TaskNow -or $TaskNow.State -ne 'Running') { break }
                        Start-Sleep -Milliseconds 150
                    }
                }
                $TaskNow = Get-ScheduledTask -TaskName $Config.Name -TaskPath $Config.TaskPath -ErrorAction SilentlyContinue
                if ($TaskNow -and $TaskNow.State -eq 'Running') {
                    Stop-ScheduledTask -TaskName $Config.Name -TaskPath $Config.TaskPath -ErrorAction SilentlyContinue
                }
            }
        }

        # Wait for the already-verified server process before re-inspecting listeners
        # so we don't race against aircd.exe exiting after the stop event.
        if ($Server -and -not $Server.Process.HasExited) {
            if (-not $Server.Process.WaitForExit(5000)) {
                Stop-AircdVerifiedProcess -Process $Server.Process -ProcessId $Server.Id
            }
        }

        # Re-check and cleanly terminate a replacement aircd.exe if still alive.
        $Remaining = Get-AircdServiceServer $Config
        if ($Remaining) {
            try {
                Stop-AircdVerifiedProcess -Process $Remaining.Process -ProcessId $Remaining.Id
            } finally {
                if ($Remaining.Process) { $Remaining.Process.Dispose() }
            }
        }

        $PortDeadline = [datetime]::UtcNow.AddSeconds(10)
        while (@(Get-AircdServiceListeners $Config).Count -gt 0) {
            if ([datetime]::UtcNow -ge $PortDeadline) {
                throw 'Server ports remain occupied after stop.'
            }
            Start-Sleep -Milliseconds 150
        }
    } finally {
        if ($Server -and $Server.Process) { $Server.Process.Dispose() }
    }
}

function Get-AircdServiceLogTail {
    param($Config)
    $Lines = @()
    foreach ($FileName in @('aircd.stderr.log', 'aircd.service.log')) {
        $Path = Join-Path $Config.LogDir $FileName
        if (Test-Path -LiteralPath $Path -PathType Leaf) {
            $Tail = @(Get-Content -LiteralPath $Path -Tail 8 -ErrorAction SilentlyContinue)
            if ($Tail.Count -gt 0) {
                $Lines += "[$FileName] " + ($Tail -join ' | ')
            }
        }
    }
    $Lines -join '; '
}

function Wait-AircdServiceReady {
    param($Config, [int]$TimeoutSeconds = 20)
    $Deadline = [datetime]::UtcNow.AddSeconds($TimeoutSeconds)
    while ([datetime]::UtcNow -lt $Deadline) {
        $Status = Test-AircdServiceHealth $Config
        if ($Status.State -eq 'Running') {
            return $Status
        }
        Start-Sleep -Milliseconds 200
    }
    $Tail = Get-AircdServiceLogTail $Config
    if ($Tail) {
        throw "Hub did not become ready on $($Config.Url) within ${TimeoutSeconds}s. $Tail"
    }
    throw "Hub did not become ready on $($Config.Url) within ${TimeoutSeconds}s."
}

function Start-AircdServiceInstance {
    param($Config, [string]$Mode = 'Auto')
    $Installation = Get-AircdServiceInstallation -Config $Config -Mode $Mode
    if (-not $Installation.Installed) {
        throw "Service/task '$($Config.Name)' is not installed. Run Install-AircdService.ps1 first."
    }
    $null = Resolve-AircdServiceExecutable $Config

    $Existing = Get-AircdServiceServer $Config
    if ($Existing) {
        return Wait-AircdServiceReady -Config $Config -TimeoutSeconds 5
    }

    if ($Installation.Mode -eq 'Service') {
        Start-Service -Name $Config.Name -ErrorAction Stop
    } else {
        Start-ScheduledTask -TaskName $Config.Name -TaskPath $Config.TaskPath -ErrorAction Stop
    }

    Wait-AircdServiceReady -Config $Config -TimeoutSeconds 20
}

function Install-AircdServiceInstance {
    param(
        $Config,
        [string]$Mode = 'Auto',
        [string]$Trigger = 'Auto',
        [switch]$RunAsSystem,
        [pscredential]$Credential,
        [switch]$SurviveLogout
    )

    $null = Resolve-AircdServiceExecutable $Config
    $Plan = Resolve-AircdServiceInstallPlan -Mode $Mode -Trigger $Trigger `
        -RunAsSystem:$RunAsSystem -Credential $Credential -SurviveLogout:$SurviveLogout

    # If the same service/task is already installed, stop it before updating the
    # host binary or switching between ScheduledTask and Service modes.
    $ExistingInstall = Get-AircdServiceInstallation -Config $Config -Mode 'Auto'
    $WasRunning = $false
    if ($ExistingInstall.Installed) {
        if ($ExistingInstall.Mode -eq 'Service' -and $ExistingInstall.Service.Status -ne 'Stopped') { $WasRunning = $true }
        if ($ExistingInstall.Mode -eq 'ScheduledTask' -and $ExistingInstall.Task.State -eq 'Running') { $WasRunning = $true }
        Stop-AircdServiceInstance -Config $Config -Mode 'Auto'
        if ($ExistingInstall.Mode -ne $Plan.Mode) {
            Uninstall-AircdServiceInstance -Config $Config -Mode $ExistingInstall.Mode -KeepServiceDir
        }
    }

    $HostExe = Initialize-AircdServiceHost -Config $Config
    $StopEventName = if ($Plan.Mode -eq 'ScheduledTask') {
        Get-AircdServiceStopEventName -Config $Config -CrossSession $Plan.CrossSession
    } else { '' }

    $HostArgs = New-Object System.Collections.Generic.List[string]
    if ($Plan.Mode -eq 'Service') {
        $HostArgs.Add('--service'); $HostArgs.Add($Config.Name)
    } else {
        $HostArgs.Add('--run')
        $HostArgs.Add('--stop-event'); $HostArgs.Add($StopEventName)
    }
    $HostArgs.Add('--repo-root'); $HostArgs.Add($Config.RepoRoot)
    $HostArgs.Add('--log-dir'); $HostArgs.Add($Config.LogDir)
    if ($Config.ExecutablePath) {
        $HostArgs.Add('--exe'); $HostArgs.Add($Config.ExecutablePath)
    }
    $HostArgs.Add('--')
    foreach ($ServerArg in $Config.ServerArguments) {
        $HostArgs.Add($ServerArg)
    }
    $HostArgString = Join-AircdServiceArguments -Values $HostArgs.ToArray()

    if ($Plan.Mode -eq 'Service') {
        $BinaryPathName = "`"$HostExe`" $HostArgString"
        $ExistingService = Get-Service -Name $Config.Name -ErrorAction SilentlyContinue
        if ($ExistingService) {
            & sc.exe config $Config.Name binPath= $BinaryPathName start= auto DisplayName= "CAMS aircd Hub ($($Config.Name))" | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "sc.exe config failed with exit code $LASTEXITCODE." }
            if ($Credential) {
                $User = $Credential.UserName
                $Pass = $Credential.GetNetworkCredential().Password
                & sc.exe config $Config.Name obj= $User password= $Pass | Out-Null
                if ($LASTEXITCODE -ne 0) { throw "sc.exe credential update failed with exit code $LASTEXITCODE." }
            }
        } else {
            $SvcParams = @{
                Name           = $Config.Name
                BinaryPathName = $BinaryPathName
                DisplayName    = "CAMS aircd Hub ($($Config.Name))"
                Description    = "Autonomous CAMS aircd coordination hub ($($Config.Url))."
                StartupType    = 'Automatic'
            }
            if ($Credential) { $SvcParams['Credential'] = $Credential }
            $null = New-Service @SvcParams
        }
        & sc.exe failure $Config.Name reset= 60 actions= restart/5000/restart/5000/restart/5000 | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "sc.exe failure policy configuration failed with exit code $LASTEXITCODE." }
        & sc.exe failureflag $Config.Name 1 | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "sc.exe failureflag configuration failed with exit code $LASTEXITCODE." }
    } else {
        $Action = New-ScheduledTaskAction -Execute $HostExe -Argument $HostArgString -WorkingDirectory $Config.RepoRoot
        $CurrentUser = [Security.Principal.WindowsIdentity]::GetCurrent().Name
        $TriggerObj = if ($Plan.Trigger -eq 'AtStartup') {
            New-ScheduledTaskTrigger -AtStartup
        } elseif ($Plan.LogonType -eq 'ServiceAccount') {
            New-ScheduledTaskTrigger -AtLogOn
        } else {
            $TriggerUser = if ($Credential) { $Credential.UserName } else { $CurrentUser }
            New-ScheduledTaskTrigger -AtLogOn -User $TriggerUser
        }

        $Settings = New-ScheduledTaskSettingsSet `
            -AllowStartIfOnBatteries `
            -DontStopIfGoingOnBatteries `
            -ExecutionTimeLimit ([TimeSpan]::Zero) `
            -MultipleInstances IgnoreNew `
            -RestartCount 3 `
            -RestartInterval (New-TimeSpan -Minutes 1) `
            -StartWhenAvailable

        $Desc = "Autonomous CAMS aircd coordination hub ($($Config.Url))."
        if ($null -ne $Credential) {
            $null = Register-ScheduledTask `
                -TaskName $Config.Name `
                -TaskPath $Config.TaskPath `
                -Action $Action `
                -Trigger $TriggerObj `
                -Settings $Settings `
                -Description $Desc `
                -User $Credential.UserName `
                -Password $Credential.GetNetworkCredential().Password `
                -Force
        } else {
            $Principal = if ($Plan.LogonType -eq 'ServiceAccount') {
                New-ScheduledTaskPrincipal -UserId 'NT AUTHORITY\SYSTEM' -LogonType ServiceAccount -RunLevel Highest
            } elseif ($Plan.LogonType -eq 'S4U') {
                New-ScheduledTaskPrincipal -UserId $CurrentUser -LogonType S4U
            } else {
                New-ScheduledTaskPrincipal -UserId $CurrentUser -LogonType Interactive
            }
            $null = Register-ScheduledTask `
                -TaskName $Config.Name `
                -TaskPath $Config.TaskPath `
                -Action $Action `
                -Trigger $TriggerObj `
                -Principal $Principal `
                -Settings $Settings `
                -Description $Desc `
                -Force
        }
    }

    Save-AircdServiceMetadata -Config $Config -InstalledMode $Plan.Mode -StopEventName $StopEventName `
        -TriggerKind $Plan.Trigger -LogonType $Plan.LogonType

    [pscustomobject]@{
        Config      = $Config
        Plan        = $Plan
        HostExe     = $HostExe
        WasRunning  = $WasRunning
    }
}

function Uninstall-AircdServiceInstance {
    param(
        $Config,
        [string]$Mode = 'Auto',
        [switch]$KeepServiceDir
    )
    $Installation = Get-AircdServiceInstallation -Config $Config -Mode $Mode
    if ($Installation.Installed) {
        Stop-AircdServiceInstance -Config $Config -Mode $Installation.Mode
        if ($Installation.Mode -eq 'Service') {
            & sc.exe delete $Config.Name | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "sc.exe delete '$($Config.Name)' failed with exit code $LASTEXITCODE." }
            $Deadline = [datetime]::UtcNow.AddSeconds(10)
            while ((Get-Service -Name $Config.Name -ErrorAction SilentlyContinue) -and [datetime]::UtcNow -lt $Deadline) {
                Start-Sleep -Milliseconds 200
            }
        } else {
            Unregister-ScheduledTask -TaskName $Config.Name -TaskPath $Config.TaskPath -Confirm:$false -ErrorAction Stop
        }
    }

    if (Test-Path -LiteralPath $Config.MetadataPath -PathType Leaf) {
        Remove-Item -LiteralPath $Config.MetadataPath -Force -ErrorAction SilentlyContinue
    }
    if (-not $KeepServiceDir -and (Test-Path -LiteralPath $Config.ServiceDir -PathType Container)) {
        $OtherMetadata = @(Get-ChildItem -LiteralPath $Config.ServiceDir -Filter '*.json' -ErrorAction SilentlyContinue)
        if ($OtherMetadata.Count -eq 0) {
            Remove-Item -LiteralPath $Config.ServiceDir -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
    $Installation
}

function Get-AircdServiceStatusReport {
    param($Config, [string]$Mode = 'Auto')
    $Installation = Get-AircdServiceInstallation -Config $Config -Mode $Mode
    $Health = Test-AircdServiceHealth $Config

    $TaskOrServiceState = 'NotInstalled'
    if ($Installation.Installed) {
        $TaskOrServiceState = if ($Installation.Mode -eq 'Service') {
            [string]$Installation.Service.Status
        } else {
            [string]$Installation.Task.State
        }
    }

    [pscustomobject]@{
        name               = $Config.Name
        installed          = $Installation.Installed
        mode               = $Installation.Mode
        installer_state    = $TaskOrServiceState
        hub_state          = $Health.State
        hub_detail         = $Health.Detail
        pid                = if ($Health.Server) { $Health.Server.Id } else { $null }
        executable         = if ($Health.Server) { $Health.Server.Executable } else { $null }
        url                = $Config.Url
        bind               = $Config.Bind
        http_bind          = $Config.HttpBind
        db_path            = $Config.DbPath
        log_dir            = $Config.LogDir
        survives_logout    = if ($Installation.Metadata) { ($Installation.Metadata.logon_type -ne 'Interactive') } else { $null }
        trigger            = if ($Installation.Metadata) { $Installation.Metadata.trigger } else { $null }
        logon_type         = if ($Installation.Metadata) { $Installation.Metadata.logon_type } else { $null }
    }
}

Export-ModuleMember -Function *-Aircd*
