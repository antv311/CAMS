$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
$TempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$TestRoot = Join-Path $TempRoot ("aircd launcher test " + [guid]::NewGuid())
$Release = Join-Path $TestRoot "target\release\aircd.exe"
$Debug = Join-Path $TestRoot "target\debug\aircd.exe"
$Resolver = Join-Path $TestRoot "scripts\Get-AircdExecutable.ps1"
$Starter = Join-Path $TestRoot "scripts\Start-Aircd.ps1"
$Wrapper = Join-Path $TestRoot "aircd-server.cmd"

function Assert-Equal($Actual, $Expected, $Label) {
    if ($Actual -ne $Expected) {
        throw "${Label}: expected '$Expected', got '$Actual'"
    }
}

try {
    New-Item -ItemType Directory -Path (Split-Path $Release), (Split-Path $Debug), (Split-Path $Resolver) -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $RepoRoot "aircd-server.cmd") -Destination $Wrapper
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot "Get-AircdExecutable.ps1") -Destination $Resolver
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot "Start-Aircd.ps1") -Destination $Starter
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
    Write-Host "PASS: Windows launcher selection, paths with spaces, arguments and exit status"
} finally {
    $ResolvedTestRoot = [IO.Path]::GetFullPath($TestRoot)
    if (!$ResolvedTestRoot.StartsWith($TempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        (Split-Path -Leaf $ResolvedTestRoot) -notlike 'aircd launcher test *') {
        throw "Refusing to remove unexpected test directory"
    }
    if (Test-Path -LiteralPath $ResolvedTestRoot) {
        Remove-Item -LiteralPath $ResolvedTestRoot -Recurse -Force
    }
}
