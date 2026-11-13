param(
    [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot)
)

# A debug build may contain fixes newer than an existing release build.
# Keep release first so it wins when the timestamps are identical.
$Candidates = @(
    (Join-Path $RepoRoot "target\release\aircd.exe"),
    (Join-Path $RepoRoot "target\debug\aircd.exe")
)
$Newest = $null
foreach ($Candidate in $Candidates) {
    if (Test-Path -LiteralPath $Candidate -PathType Leaf) {
        $File = Get-Item -LiteralPath $Candidate
        if ($null -eq $Newest -or $File.LastWriteTimeUtc -gt $Newest.LastWriteTimeUtc) {
            $Newest = $File
        }
    }
}
if ($null -ne $Newest) {
    $Newest.FullName
}
