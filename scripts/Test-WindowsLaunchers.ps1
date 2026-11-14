$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
$TempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$TestRoot = Join-Path $TempRoot ("aircd launcher test " + [guid]::NewGuid())
$Release = Join-Path $TestRoot "target\release\aircd.exe"
$Debug = Join-Path $TestRoot "target\debug\aircd.exe"
$Resolver = Join-Path $TestRoot "scripts\Get-AircdExecutable.ps1"
$Starter = Join-Path $TestRoot "scripts\Start-Aircd.ps1"
$ConfigMod = Join-Path $TestRoot "scripts\AircdConfig.psm1"
$Wrapper = Join-Path $TestRoot "aircd-server.cmd"

$SavedEnv = @{}
foreach ($EnvName in @("AIRCD_CONFIG_DIR", "AIRCD_DATA_DIR", "AIRCD_CONFIG", "AIRCD_WINDOWS_CONFIG", "AIRCD_EXECUTABLE", "AIRCD_BIND", "AIRCD_HTTP_BIND", "AIRCD_DB", "AIRCD_OPEN_REGISTRATION", "AIRCD_REGISTER_KEY", "AIRCD_WINDOWS_EVENT_LOG", "AIRCD_WINDOWS_EVENT_LOG_SOURCE", "AIRCD_SYSLOG_TARGET", "AIRCD_SYSLOG_FORMAT", "AIRCD_SYSLOG_FACILITY")) {
    $Item = Get-Item -LiteralPath "Env:\$EnvName" -ErrorAction SilentlyContinue
    $SavedEnv[$EnvName] = if ($null -ne $Item) { $Item.Value } else { $null }
    Remove-Item -LiteralPath "Env:\$EnvName" -ErrorAction SilentlyContinue
}

function Assert-Equal($Actual, $Expected, $Label) {
    if ($Actual -ne $Expected) {
        throw "${Label}: expected '$Expected', got '$Actual'"
    }
}

try {
    $IsolatedCfgDir = Join-Path $TestRoot "isolated-config"
    New-Item -ItemType Directory -Path (Split-Path $Release), (Split-Path $Debug), (Split-Path $Resolver), $IsolatedCfgDir -Force | Out-Null
    $env:AIRCD_CONFIG_DIR = $IsolatedCfgDir
    Copy-Item -LiteralPath (Join-Path $RepoRoot "aircd-server.cmd") -Destination $Wrapper
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot "Get-AircdExecutable.ps1") -Destination $Resolver
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot "Start-Aircd.ps1") -Destination $Starter
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot "AircdConfig.psm1") -Destination $ConfigMod
    Assert-Equal (& $Resolver) $null "no build"

    # A real executable exercises both wrappers, argument quoting and exit codes.
    Add-Type -OutputAssembly $Release -OutputType ConsoleApplication -TypeDefinition @'
public static class LauncherFixture {
    public static int Main(string[] args) {
        System.Console.WriteLine(System.IO.Path.GetFileName(System.IO.Path.GetDirectoryName(
            System.Reflection.Assembly.GetExecutingAssembly().Location)));
        foreach (string arg in args) System.Console.WriteLine(arg);
        return 17;
    }
}
'@
    Assert-Equal (& $Resolver) $Release "release only"
    Move-Item -LiteralPath $Release -Destination $Debug
    Assert-Equal (& $Resolver) $Debug "debug only"
    Copy-Item -LiteralPath $Debug -Destination $Release
    $Older = [datetime]::UtcNow.AddMinutes(-10)
    $Newer = $Older.AddMinutes(1)
    (Get-Item -LiteralPath $Release).LastWriteTimeUtc = $Older
    (Get-Item -LiteralPath $Debug).LastWriteTimeUtc = $Newer
    Assert-Equal (& $Resolver) $Debug "newer debug"
    $Output = & $Wrapper --test "argument with spaces"
    Assert-Equal $LASTEXITCODE 17 "cmd exit status"
    Assert-Equal ($Output -join '|') 'debug|--test|argument with spaces' "cmd arguments"
    $Db = Join-Path $TestRoot "data folder\test.sqlite3"
    $Output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Starter -Bind 127.0.0.1:16667 -HttpBind 127.0.0.1:16680 -DbPath $Db
    Assert-Equal $LASTEXITCODE 17 "PowerShell exit status"
    Assert-Equal ($Output -join '|') "debug|--bind|127.0.0.1:16667|--http-bind|127.0.0.1:16680|--db|$Db" "PowerShell arguments"

    (Get-Item -LiteralPath $Release).LastWriteTimeUtc = $Newer.AddMinutes(1)
    Assert-Equal (& $Resolver) $Release "newer release"
    $Output = & $Wrapper --test
    Assert-Equal ($Output -join '|') 'release|--test' "cmd newer release"
    (Get-Item -LiteralPath $Debug).LastWriteTimeUtc = (Get-Item -LiteralPath $Release).LastWriteTimeUtc
    Assert-Equal (& $Resolver) $Release "tie prefers release"

    # AIRCD_EXECUTABLE override in aircd-server.cmd
    $env:AIRCD_EXECUTABLE = $Debug
    $Output = & $Wrapper --env-exe
    Assert-Equal ($Output -join '|') 'debug|--env-exe' "cmd AIRCD_EXECUTABLE override"
    Remove-Item Env:\AIRCD_EXECUTABLE -ErrorAction SilentlyContinue

    # Editable JSON config + env + CLI precedence in Start-Aircd.ps1
    $JsonCfgPath = Join-Path $TestRoot "custom-win.json"
    $RelDbExpected = [IO.Path]::GetFullPath((Join-Path $TestRoot "rel-data\from-json.sqlite3"))
    @{
        server = @{
            bind = "127.0.0.1:17667"
            http_bind = "127.0.0.1:17680"
            db_path = "rel-data\from-json.sqlite3"
            open_registration = $true
        }
    } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $JsonCfgPath -Encoding UTF8

    $Output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Starter -ConfigPath $JsonCfgPath
    Assert-Equal $LASTEXITCODE 17 "JSON config exit status"
    Assert-Equal ($Output -join '|') "release|--bind|127.0.0.1:17667|--http-bind|127.0.0.1:17680|--db|$RelDbExpected|--open-registration" "JSON config resolution"

    # Editable TOML config via AIRCD_CONFIG_DIR + env > file + CLI > env
    $TomlCfgPath = Join-Path $IsolatedCfgDir "aircd.toml"
    $TomlDb = Join-Path $TestRoot "toml-data\from-toml.sqlite3"
    @"
# Comment
[server]
bind = "127.0.0.1:18667"
http_bind = "127.0.0.1:18680"
db_path = '$TomlDb'
open_registration = false
"@ | Set-Content -LiteralPath $TomlCfgPath -Encoding UTF8

    $env:AIRCD_HTTP_BIND = "127.0.0.1:19680"
    $Output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Starter -Bind 127.0.0.1:20667
    Assert-Equal $LASTEXITCODE 17 "TOML/env/CLI precedence exit status"
    Assert-Equal ($Output -join '|') "release|--bind|127.0.0.1:20667|--http-bind|127.0.0.1:19680|--db|$TomlDb" "CLI > Env > TOML precedence"

    # Logging forwarding config & env/CLI precedence in Start-Aircd.ps1
    $env:AIRCD_SYSLOG_TARGET = "tcp://127.0.0.1:6514"
    $env:AIRCD_SYSLOG_FORMAT = "rfc3164"
    $Output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Starter -Bind 127.0.0.1:20667 -WindowsEventLog -WindowsEventLogSource "cams-hub" -SyslogFacility "local4"
    Assert-Equal $LASTEXITCODE 17 "Logging forwarding exit status"
    Assert-Equal ($Output -join '|') "release|--bind|127.0.0.1:20667|--http-bind|127.0.0.1:19680|--db|$TomlDb|--windows-event-log|--windows-event-log-source|cams-hub|--syslog-target|tcp://127.0.0.1:6514|--syslog-format|rfc3164|--syslog-facility|local4" "Logging forwarding flags"

    Write-Host "PASS: Windows launcher selection, paths with spaces, editable JSON/TOML config & env/CLI precedence, arguments and exit status"
} finally {
    foreach ($EnvName in $SavedEnv.Keys) {
        if ($null -eq $SavedEnv[$EnvName]) {
            Remove-Item -LiteralPath "Env:\$EnvName" -ErrorAction SilentlyContinue
        } else {
            Set-Item -LiteralPath "Env:\$EnvName" -Value $SavedEnv[$EnvName]
        }
    }
    $ResolvedTestRoot = [IO.Path]::GetFullPath($TestRoot)
    if (!$ResolvedTestRoot.StartsWith($TempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        (Split-Path -Leaf $ResolvedTestRoot) -notlike 'aircd launcher test *') {
        throw "Refusing to remove unexpected test directory"
    }
    if (Test-Path -LiteralPath $ResolvedTestRoot) {
        Remove-Item -LiteralPath $ResolvedTestRoot -Recurse -Force
    }
}
