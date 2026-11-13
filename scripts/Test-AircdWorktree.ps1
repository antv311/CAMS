$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
$Script = Join-Path $PSScriptRoot "New-AircdWorktree.ps1"
$TempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$TestRoot = Join-Path $TempRoot ("aircd worktree test " + [guid]::NewGuid())

function Assert-Equal($Actual, $Expected, $Label) {
    if ($Actual -ne $Expected) {
        throw "${Label}: expected '$Expected', got '$Actual'"
    }
}

function Assert-True($Condition, $Label) {
    if (-not $Condition) {
        throw "Assertion failed: $Label"
    }
}

function Invoke-GitChecked([string]$Root, [string[]]$GitArgs) {
    $Prev = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $Out = & git -C $Root @GitArgs 2>&1
        $Code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $Prev
    }
    if ($Code -ne 0) {
        throw "git $($GitArgs -join ' ') failed: $($Out -join "`n")"
    }
    ($Out | ForEach-Object { "$_" }) -join "`n"
}

try {
    New-Item -ItemType Directory -Path $TestRoot -Force | Out-Null
    $null = Invoke-GitChecked $TestRoot @("init", "-b", "main")
    $null = Invoke-GitChecked $TestRoot @("config", "user.email", "tester@example.local")
    $null = Invoke-GitChecked $TestRoot @("config", "user.name", "Tester")
    "init" | Set-Content -LiteralPath (Join-Path $TestRoot "README.md") -Encoding UTF8
    $null = Invoke-GitChecked $TestRoot @("add", "README.md")
    $null = Invoke-GitChecked $TestRoot @("commit", "-m", "initial commit")

    # 1. Create a worktree at .aircd/worktrees/<branch>
    $CreatedJson = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Script -RepoRoot $TestRoot -Branch "feat/sample-task" -Json
    Assert-Equal $LASTEXITCODE 0 "create exit code"
    $Created = $CreatedJson | ConvertFrom-Json
    $ExpectedWt = [IO.Path]::GetFullPath((Join-Path $TestRoot ".aircd\worktrees\feat\sample-task"))
    Assert-Equal $Created.ok $true "created ok"
    Assert-Equal $Created.created $true "created flag"
    Assert-Equal $Created.reused $false "reused flag"
    Assert-Equal $Created.worktree $ExpectedWt "worktree path"
    Assert-True (Test-Path -LiteralPath (Join-Path $ExpectedWt ".git")) "worktree .git exists"
    $WtBranch = (Invoke-GitChecked $ExpectedWt @("rev-parse", "--abbrev-ref", "HEAD")).Trim()
    Assert-Equal $WtBranch "feat/sample-task" "worktree branch"

    # 2. Re-running is idempotent and reports reused=true
    $ReusedJson = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Script -RepoRoot $TestRoot -Branch "feat/sample-task" -Json
    Assert-Equal $LASTEXITCODE 0 "reuse exit code"
    $Reused = $ReusedJson | ConvertFrom-Json
    Assert-Equal $Reused.created $false "reused created flag"
    Assert-Equal $Reused.reused $true "reused flag"
    Assert-Equal $Reused.worktree $ExpectedWt "reused path"

    # 3. Invoking from inside an existing worktree still anchors under primary repo root
    Push-Location $ExpectedWt
    try {
        $SecondJson = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Script -TaskId "task_abc" -Json
        Assert-Equal $LASTEXITCODE 0 "nested invocation exit code"
        $Second = $SecondJson | ConvertFrom-Json
        $ExpectedSecond = [IO.Path]::GetFullPath((Join-Path $TestRoot ".aircd\worktrees\task\task_abc"))
        Assert-Equal $Second.branch "task/task_abc" "default branch from TaskId"
        Assert-Equal $Second.worktree $ExpectedSecond "primary repo root worktree"
        Assert-True (Test-Path -LiteralPath $ExpectedSecond) "second worktree exists"
    } finally {
        Pop-Location
    }

    # 4. List worktrees
    $ListJson = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Script -RepoRoot $TestRoot -List -Json
    Assert-Equal $LASTEXITCODE 0 "list exit code"
    $Listed = $ListJson | ConvertFrom-Json
    Assert-Equal $Listed.worktrees.Count 3 "worktree count"

    # 5. Unsafe branch names fail
    $PrevPref = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $null = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Script -RepoRoot $TestRoot -Branch "../escape" 2>&1
    $BadExit = $LASTEXITCODE
    $ErrorActionPreference = $PrevPref
    Assert-True ($BadExit -ne 0) "unsafe branch rejected"

    # 6. Remove worktrees and delete branches
    $RmJson = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Script -RepoRoot $TestRoot -Branch "feat/sample-task" -Remove -DeleteBranch -Json
    Assert-Equal $LASTEXITCODE 0 "remove exit code"
    $Removed = $RmJson | ConvertFrom-Json
    Assert-Equal $Removed.removed $true "removed flag"
    Assert-True (-not (Test-Path -LiteralPath $ExpectedWt)) "removed worktree directory gone"

    $Rm2Json = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Script -RepoRoot $TestRoot -TaskId "task_abc" -Remove -DeleteBranch -Json
    Assert-Equal $LASTEXITCODE 0 "remove second exit code"
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $TestRoot ".aircd\worktrees\task\task_abc"))) "second worktree gone"

    Write-Host "PASS: New-AircdWorktree lifecycle (create, idempotent reuse, nested root resolution, list, safety, remove)"
} finally {
    $ResolvedTestRoot = [IO.Path]::GetFullPath($TestRoot)
    if (-not $ResolvedTestRoot.StartsWith($TempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        (Split-Path -Leaf $ResolvedTestRoot) -notlike 'aircd worktree test *') {
        throw "Refusing to remove unexpected test directory"
    }
    if (Test-Path -LiteralPath $ResolvedTestRoot) {
        Remove-Item -LiteralPath $ResolvedTestRoot -Recurse -Force
    }
}
