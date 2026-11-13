param(
    [switch]$Uninstall,
    [switch]$StartNow,
    [string]$Bind = '127.0.0.1:6667',
    [string]$HttpBind = '127.0.0.1:6680',
    [string]$DbPath = "$env:LOCALAPPDATA\aircd\aircd.sqlite3",
    [string]$Profile = '',
    [string]$StartupDirectory = [Environment]::GetFolderPath('Startup')
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'AircdTray.psm1') -Force
$Config = New-AircdTrayConfig -RepoRoot (Split-Path -Parent $PSScriptRoot) -Bind $Bind -HttpBind $HttpBind -DbPath $DbPath -Profile $Profile
$ShortcutPath = Join-Path $StartupDirectory 'CAMS tray.lnk'
if ($Uninstall) {
    if (Test-Path -LiteralPath $ShortcutPath) { Remove-Item -LiteralPath $ShortcutPath }
    Write-Host 'Removed CAMS tray sign-in shortcut. The running tray and server are unchanged.'
    exit 0
}
New-Item -ItemType Directory -Path $StartupDirectory -Force | Out-Null
$TrayScript = Join-Path $PSScriptRoot 'Start-AircdTray.ps1'
$Arguments = @('-NoProfile', '-STA', '-WindowStyle', 'Hidden', '-ExecutionPolicy', 'Bypass', '-File',
    $TrayScript, '-Bind', $Bind, '-HttpBind', $HttpBind, '-DbPath', $Config.DbPath)
if ($Profile) { $Arguments += @('-Profile', $Profile) }
$Shell = New-Object -ComObject WScript.Shell
$Shortcut = $Shell.CreateShortcut($ShortcutPath)
$Shortcut.TargetPath = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
$Shortcut.Arguments = Join-AircdArguments $Arguments
$Shortcut.WorkingDirectory = $Config.RepoRoot
$Shortcut.Description = 'CAMS server controls and TUI launcher'
$Shortcut.WindowStyle = 7
$Shortcut.Save()

# Migrate the known legacy hub shortcut only when it runs this checkout's
# Start-Aircd.ps1. Preserve a backup so uninstall can be reversed manually.
$LegacyPath = Join-Path $StartupDirectory 'CAMS hub.lnk'
if (Test-Path -LiteralPath $LegacyPath) {
    $Legacy = $Shell.CreateShortcut($LegacyPath)
    $LegacyScript = Join-Path $PSScriptRoot 'Start-Aircd.ps1'
    if ($Legacy.Arguments.IndexOf($LegacyScript, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
        $Backup = "$LegacyPath.before-tray"
        if (!(Test-Path -LiteralPath $Backup)) { Copy-Item -LiteralPath $LegacyPath -Destination $Backup }
        Remove-Item -LiteralPath $LegacyPath
        Write-Host "Replaced legacy hub shortcut (backup: $Backup)"
    } else { Write-Warning 'CAMS hub.lnk targets another checkout; it was left unchanged. Remove duplicate server startup entries if needed.' }
}
Write-Host "CAMS tray will start at sign-in: $ShortcutPath"
if ($StartNow) {
    $null = Start-Process -FilePath $Shortcut.TargetPath -ArgumentList $Shortcut.Arguments -WorkingDirectory $Config.RepoRoot -WindowStyle Hidden
    Write-Host 'Started CAMS tray. Right-click its C icon for Restart Server or Launch TUI.'
}
