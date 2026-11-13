param(
    [string]$Branch,
    [string]$Base,
    [string]$RepoRoot,
    [string]$WorktreeDir,
    [string]$TaskId,
    [switch]$Remove,
    [switch]$DeleteBranch,
    [switch]$Force,
    [switch]$List,
    [switch]$Json
)

$ErrorActionPreference = "Stop"

function Invoke-Git {
    param(
        [string]$Root,
        [string[]]$GitArgs,
        [switch]$IgnoreExitCode
    )
    $PrevPref = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $Output = & git -C $Root @GitArgs 2>&1
        $Code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $PrevPref
    }
    $Text = ($Output | ForEach-Object { "$_" }) -join "`n"
    if (-not $IgnoreExitCode -and $Code -ne 0) {
        throw "git $($GitArgs -join ' ') failed (exit $Code): $($Text.Trim())"
    }
    [PSCustomObject]@{
        ExitCode = $Code
        Output   = $Text.Trim()
    }
}

function Resolve-NormalizedPath {
    param([string]$PathValue)
    [IO.Path]::GetFullPath($PathValue).TrimEnd('\', '/')
}

function Get-GitWorktrees {
    param([string]$Root)
    $Res = Invoke-Git -Root $Root -GitArgs @("worktree", "list", "--porcelain")
    $Items = @()
    $Current = $null
    foreach ($RawLine in ($Res.Output -split "`r?`n")) {
        $Line = $RawLine.Trim()
        if ([string]::IsNullOrEmpty($Line)) {
            if ($null -ne $Current) {
                $Items += $Current
                $Current = $null
            }
            continue
        }
        if ($Line.StartsWith("worktree ")) {
            if ($null -ne $Current) { $Items += $Current }
            $PathVal = $Line.Substring(9).Trim()
            $Current = [ordered]@{
                path   = (Resolve-NormalizedPath $PathVal)
                head   = $null
                branch = $null
                bare   = $false
            }
        } elseif ($null -ne $Current) {
            if ($Line.StartsWith("HEAD ")) {
                $Current.head = $Line.Substring(5).Trim()
            } elseif ($Line.StartsWith("branch ")) {
                $Ref = $Line.Substring(7).Trim()
                if ($Ref.StartsWith("refs/heads/")) {
                    $Ref = $Ref.Substring(11)
                }
                $Current.branch = $Ref
            } elseif ($Line -eq "bare") {
                $Current.bare = $true
            }
        }
    }
    if ($null -ne $Current) { $Items += $Current }
    return ,$Items
}

function Assert-SafeBranch {
    param(
        [string]$Root,
        [string]$BranchName
    )
    if ([string]::IsNullOrWhiteSpace($BranchName)) {
        throw "Branch name is required (pass -Branch or -TaskId)."
    }
    if ($BranchName.StartsWith("/") -or $BranchName.StartsWith("\") -or $BranchName.Contains("..") -or $BranchName.Contains(":")) {
        throw "Unsafe branch name '$BranchName'."
    }
    $Check = Invoke-Git -Root $Root -GitArgs @("check-ref-format", "--branch", $BranchName) -IgnoreExitCode
    if ($Check.ExitCode -ne 0) {
        throw "Invalid git branch name '$BranchName': $($Check.Output)"
    }
}

$SeedDir = if ($RepoRoot) { $RepoRoot } else { (Get-Location).Path }
if (-not (Test-Path -LiteralPath $SeedDir)) {
    throw "RepoRoot '$SeedDir' does not exist."
}
$TopRes = Invoke-Git -Root $SeedDir -GitArgs @("rev-parse", "--show-toplevel") -IgnoreExitCode
if ($TopRes.ExitCode -ne 0) {
    if (-not $RepoRoot) {
        $Fallback = Split-Path -Parent $PSScriptRoot
        $TopRes = Invoke-Git -Root $Fallback -GitArgs @("rev-parse", "--show-toplevel") -IgnoreExitCode
    }
    if ($TopRes.ExitCode -ne 0) {
        throw "Not inside a git repository: $SeedDir"
    }
}
$ResolvedRoot = Resolve-NormalizedPath $TopRes.Output
$CommonDirRes = Invoke-Git -Root $ResolvedRoot -GitArgs @("rev-parse", "--git-common-dir")
$CommonDirRaw = $CommonDirRes.Output
if (-not [IO.Path]::IsPathRooted($CommonDirRaw)) {
    $CommonDirRaw = Join-Path $ResolvedRoot $CommonDirRaw
}
$CommonDir = Resolve-NormalizedPath $CommonDirRaw
if ((Split-Path -Leaf $CommonDir) -eq ".git") {
    $ResolvedRoot = Resolve-NormalizedPath (Split-Path -Parent $CommonDir)
}

if ($List) {
    $Worktrees = Get-GitWorktrees -Root $ResolvedRoot
    if ($Json) {
        [ordered]@{
            ok        = $true
            repo_root = $ResolvedRoot
            worktrees = $Worktrees
        } | ConvertTo-Json -Depth 4
    } else {
        foreach ($Wt in $Worktrees) {
            $Br = if ($Wt.branch) { $Wt.branch } else { "(detached)" }
            Write-Output "$($Wt.path)`t[$Br]"
        }
    }
    exit 0
}

if ([string]::IsNullOrWhiteSpace($Branch) -and -not [string]::IsNullOrWhiteSpace($TaskId)) {
    $Branch = "task/$($TaskId.Trim())"
}
$Branch = $Branch.Trim()
Assert-SafeBranch -Root $ResolvedRoot -BranchName $Branch

$BranchParts = $Branch -split '[/\\]' | Where-Object { $_ -ne '' }
$DefaultTarget = Join-Path $ResolvedRoot ".aircd\worktrees"
foreach ($Part in $BranchParts) {
    $DefaultTarget = Join-Path $DefaultTarget $Part
}
$TargetDir = Resolve-NormalizedPath $(if ($WorktreeDir) { $WorktreeDir } else { $DefaultTarget })

$ExistingWorktrees = Get-GitWorktrees -Root $ResolvedRoot
$MatchingWt = $ExistingWorktrees | Where-Object {
    [string]::Equals($_.path, $TargetDir, [StringComparison]::OrdinalIgnoreCase)
} | Select-Object -First 1

if ($Remove) {
    if ($null -ne $MatchingWt -or (Test-Path -LiteralPath $TargetDir)) {
        $RmArgs = @("worktree", "remove")
        if ($Force) { $RmArgs += "--force" }
        $RmArgs += $TargetDir
        $null = Invoke-Git -Root $ResolvedRoot -GitArgs $RmArgs
    }
    $null = Invoke-Git -Root $ResolvedRoot -GitArgs @("worktree", "prune")
    if ($DeleteBranch) {
        $BranchCheck = Invoke-Git -Root $ResolvedRoot -GitArgs @("show-ref", "--verify", "--quiet", "refs/heads/$Branch") -IgnoreExitCode
        if ($BranchCheck.ExitCode -eq 0) {
            $DelFlag = if ($Force) { "-D" } else { "-d" }
            $null = Invoke-Git -Root $ResolvedRoot -GitArgs @("branch", $DelFlag, $Branch)
        }
    }
    if ($Json) {
        [ordered]@{
            ok        = $true
            removed   = $true
            repo_root = $ResolvedRoot
            branch    = $Branch
            worktree  = $TargetDir
        } | ConvertTo-Json -Depth 4
    } else {
        Write-Output $TargetDir
    }
    exit 0
}

$Created = $false
$Reused = $false
if ($null -ne $MatchingWt) {
    if ($MatchingWt.branch -and $MatchingWt.branch -ne $Branch) {
        throw "Worktree '$TargetDir' is already on branch '$($MatchingWt.branch)', expected '$Branch'."
    }
    $Reused = $true
} else {
    if (Test-Path -LiteralPath $TargetDir) {
        $Entries = Get-ChildItem -LiteralPath $TargetDir -Force -ErrorAction SilentlyContinue
        if ($Entries.Count -gt 0) {
            throw "Target directory '$TargetDir' already exists and is not a registered git worktree."
        }
    }
    $ParentDir = Split-Path -Parent $TargetDir
    if ($ParentDir -and -not (Test-Path -LiteralPath $ParentDir)) {
        New-Item -ItemType Directory -Path $ParentDir -Force | Out-Null
    }
    $BranchExists = (Invoke-Git -Root $ResolvedRoot -GitArgs @("show-ref", "--verify", "--quiet", "refs/heads/$Branch") -IgnoreExitCode).ExitCode -eq 0
    if ($BranchExists) {
        $null = Invoke-Git -Root $ResolvedRoot -GitArgs @("worktree", "add", $TargetDir, $Branch)
    } else {
        $BaseRef = $Base
        if ([string]::IsNullOrWhiteSpace($BaseRef)) {
            $HasMain = (Invoke-Git -Root $ResolvedRoot -GitArgs @("show-ref", "--verify", "--quiet", "refs/heads/main") -IgnoreExitCode).ExitCode -eq 0
            $BaseRef = if ($HasMain) { "main" } else { "HEAD" }
        }
        $null = Invoke-Git -Root $ResolvedRoot -GitArgs @("worktree", "add", "-b", $Branch, $TargetDir, $BaseRef)
    }
    $Created = $true
}

if ($Json) {
    [ordered]@{
        ok        = $true
        repo_root = $ResolvedRoot
        branch    = $Branch
        worktree  = $TargetDir
        task_id   = if ($TaskId) { $TaskId } else { $null }
        created   = $Created
        reused    = $Reused
    } | ConvertTo-Json -Depth 4
} else {
    Write-Output $TargetDir
}
