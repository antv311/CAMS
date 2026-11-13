param(
    [string]$Bind = '127.0.0.1:6667',
    [string]$HttpBind = '127.0.0.1:6680',
    [string]$DbPath = "$env:LOCALAPPDATA\aircd\aircd.sqlite3",
    [string]$Profile = '',
    [switch]$NoStart
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'AircdTray.psm1') -Force
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
[Windows.Forms.Application]::EnableVisualStyles()
$Config = New-AircdTrayConfig -RepoRoot (Split-Path -Parent $PSScriptRoot) -Bind $Bind -HttpBind $HttpBind -DbPath $DbPath -Profile $Profile
$Mutex = New-Object Threading.Mutex($false, (Get-AircdTrayMutexName $Config))
$OwnsMutex = $false
try {
    try { $OwnsMutex = $Mutex.WaitOne(0) } catch [Threading.AbandonedMutexException] { $OwnsMutex = $true }
    if (!$OwnsMutex) { Write-Host 'A CAMS tray is already controlling these ports.'; exit 0 }
    Initialize-AircdNative
    $Icons = @{}
    foreach ($Pair in @(@('Running', 'MediumSeaGreen'), @('Busy', 'Goldenrod'), @('Stopped', 'Gray'), @('Error', 'IndianRed'))) {
        $Bitmap = New-Object Drawing.Bitmap(32, 32)
        $Graphics = [Drawing.Graphics]::FromImage($Bitmap)
        $Graphics.SmoothingMode = 'AntiAlias'
        $Brush = New-Object Drawing.SolidBrush([Drawing.Color]::FromName($Pair[1]))
        $Graphics.FillEllipse($Brush, 2, 2, 28, 28)
        $Font = New-Object Drawing.Font('Segoe UI', 17, [Drawing.FontStyle]::Bold)
        $Graphics.DrawString('C', $Font, [Drawing.Brushes]::White, 4, 1)
        $Handle = $Bitmap.GetHicon()
        $BorrowedIcon = [Drawing.Icon]::FromHandle($Handle)
        $Icons[$Pair[0]] = $BorrowedIcon.Clone()
        $BorrowedIcon.Dispose()
        $null = [AircdTray.Native]::DestroyIcon($Handle)
        $Font.Dispose(); $Brush.Dispose(); $Graphics.Dispose(); $Bitmap.Dispose()
    }
    $Notify = New-Object Windows.Forms.NotifyIcon
    $Notify.Icon = $Icons['Busy']
    $Notify.Text = 'CAMS - checking server'
    $Notify.Visible = $true
    $Context = New-Object Windows.Forms.ApplicationContext
    $script:Worker = $null
    $script:Pending = if ($NoStart) { 'Status' } else { 'Start' }
    $script:LastStatus = $null
    $script:NextPoll = [datetime]::MinValue
    $script:LastError = ''
    $script:LastDisplay = ''

    function Set-TrayState([string]$State, [string]$Detail) {
        $Notify.Icon = $Icons[$State]
        $Notify.Text = ("CAMS - $Detail").Substring(0, [Math]::Min(63, ("CAMS - $Detail").Length))
        $Menu.Items[0].Text = $Detail
        $Display = "$State`: $Detail"
        if ($Display -ne $script:LastDisplay) {
            Write-AircdTrayLog $Config "Tray $Display"
            $script:LastDisplay = $Display
        }
    }

    $Menu = New-AircdTrayMenu -OnAction {
        param($Action)
        switch ($Action) {
            'Exit' { $Context.ExitThread() }
            'Logs' { $null = Start-Process explorer.exe -ArgumentList (Join-AircdArguments @($Config.LogDir)) }
            'Details' {
                $Details = "Hub: $($Config.Url)`nDatabase: $($Config.DbPath)"
                if ($script:LastStatus -and $script:LastStatus.Server) {
                    $Server = $script:LastStatus.Server
                    $Details += "`nPID: $($Server.Id)`nExecutable: $($Server.Executable)`nStarted: $($Server.Started)"
                }
                if ($script:LastError) { $Details += "`nError: $script:LastError" }
                $null = [Windows.Forms.MessageBox]::Show($Details, 'CAMS Server Details')
            }
            default { if (!$script:Worker) { $script:Pending = $Action } }
        }
    }
    $Notify.ContextMenuStrip = $Menu
    $Notify.add_DoubleClick({ if (!$script:Worker) { $script:Pending = 'Tui' } })
    $Timer = New-Object Windows.Forms.Timer
    $Timer.Interval = 500
    $Timer.add_Tick({
        try {
            if ($script:Worker) {
                if (!$script:Worker.Async.IsCompleted) { return }
                $Completed = $script:Worker
                $script:Worker = $null
                try {
                    $Result = @($Completed.Shell.EndInvoke($Completed.Async))[-1]
                    if (!$Result.Ok) { throw $Result.Error }
                    $Config.ServerArguments = $Result.Arguments
                    $script:LastStatus = $Result.Status
                    $script:LastError = ''
                    Set-TrayState $Result.Status.State $Result.Status.Detail
                } finally { $Completed.Shell.Dispose() }
                $script:NextPoll = [datetime]::UtcNow.AddSeconds(3)
                foreach ($Item in $Menu.Items) { if ($Item.Tag) { $Item.Enabled = $true } }
            }
            if (!$script:Pending -and [datetime]::UtcNow -ge $script:NextPoll) { $script:Pending = 'Status' }
            if ($script:Pending) {
                $Action = $script:Pending
                $script:Pending = $null
                if ($Action -ne 'Status') { Set-TrayState 'Busy' "$Action in progress..." }
                foreach ($Item in $Menu.Items) { if ($Item.Tag -in @('Restart', 'Stop', 'Tui', 'Exit')) { $Item.Enabled = $false } }
                $Shell = [PowerShell]::Create()
                $null = $Shell.AddScript({
                    param($Module, $Settings, $Command)
                    try {
                        Import-Module $Module -Force
                        $Status = Invoke-AircdTrayAction $Settings $Command
                        [pscustomobject]@{ Ok = $true; Status = $Status; Arguments = $Settings.ServerArguments }
                    } catch { [pscustomobject]@{ Ok = $false; Error = $_.Exception.Message } }
                }).AddArgument((Join-Path $PSScriptRoot 'AircdTray.psm1')).AddArgument($Config).AddArgument($Action)
                $script:Worker = @{ Shell = $Shell; Async = $Shell.BeginInvoke() }
            }
        } catch {
            $script:LastError = $_.Exception.Message
            Set-TrayState 'Error' 'Server error - see Server Details'
            Write-AircdTrayLog $Config $script:LastError
            $Notify.BalloonTipTitle = 'CAMS server'
            $Notify.BalloonTipText = $script:LastError
            $Notify.ShowBalloonTip(5000)
            $script:NextPoll = [datetime]::UtcNow.AddSeconds(10)
            foreach ($Item in $Menu.Items) { if ($Item.Tag) { $Item.Enabled = $true } }
        }
    })
    $Timer.Start()
    [Windows.Forms.Application]::Run($Context)
} catch {
    Write-AircdTrayLog $Config $_.Exception.Message
    $null = [Windows.Forms.MessageBox]::Show($_.Exception.Message, 'CAMS tray could not start')
    exit 1
} finally {
    if (Get-Variable Timer -ErrorAction SilentlyContinue) { $Timer.Stop(); $Timer.Dispose() }
    if (Get-Variable Notify -ErrorAction SilentlyContinue) { $Notify.Visible = $false; $Notify.Dispose() }
    if (Get-Variable Menu -ErrorAction SilentlyContinue) { $Menu.Dispose() }
    if (Get-Variable Icons -ErrorAction SilentlyContinue) { foreach ($Icon in $Icons.Values) { $Icon.Dispose() } }
    if ($OwnsMutex) { $Mutex.ReleaseMutex() }
    $Mutex.Dispose()
}
