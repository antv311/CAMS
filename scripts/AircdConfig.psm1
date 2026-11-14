Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-AircdDefaultDataDir {
    param([string]$RepoRoot = '')

    if (-not [string]::IsNullOrWhiteSpace($env:AIRCD_DATA_DIR)) {
        return [IO.Path]::GetFullPath($env:AIRCD_DATA_DIR.Trim())
    }
    if (-not [string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        return [IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA.Trim() 'aircd'))
    }
    $LocalAppData = [Environment]::GetFolderPath('LocalApplicationData')
    if (-not [string]::IsNullOrWhiteSpace($LocalAppData)) {
        return [IO.Path]::GetFullPath((Join-Path $LocalAppData 'aircd'))
    }
    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        return [IO.Path]::GetFullPath((Join-Path $env:USERPROFILE.Trim() 'AppData\Local\aircd'))
    }
    if (-not [string]::IsNullOrWhiteSpace($RepoRoot)) {
        return [IO.Path]::GetFullPath((Join-Path $RepoRoot '.aircd'))
    }
    return [IO.Path]::GetFullPath((Join-Path (Get-Location).Path '.aircd'))
}

function Get-AircdDefaultConfigDir {
    param([string]$RepoRoot = '')

    if (-not [string]::IsNullOrWhiteSpace($env:AIRCD_CONFIG_DIR)) {
        return [IO.Path]::GetFullPath($env:AIRCD_CONFIG_DIR.Trim())
    }
    if (-not [string]::IsNullOrWhiteSpace($env:XDG_CONFIG_HOME)) {
        return [IO.Path]::GetFullPath((Join-Path $env:XDG_CONFIG_HOME.Trim() 'aircd'))
    }
    return Get-AircdDefaultDataDir -RepoRoot $RepoRoot
}

function Expand-AircdConfigString {
    param([string]$Value)
    if ($null -eq $Value) { return '' }
    [Environment]::ExpandEnvironmentVariables($Value)
}

function Resolve-AircdConfigPathValue {
    param(
        [string]$Value,
        [string]$BaseDir = ''
    )
    if ([string]::IsNullOrWhiteSpace($Value)) { return '' }
    $Expanded = (Expand-AircdConfigString -Value $Value).Trim()
    if ($Expanded -eq ':memory:') { return ':memory:' }
    if ([IO.Path]::IsPathRooted($Expanded)) {
        return [IO.Path]::GetFullPath($Expanded)
    }
    if (-not [string]::IsNullOrWhiteSpace($BaseDir)) {
        return [IO.Path]::GetFullPath((Join-Path $BaseDir $Expanded))
    }
    return [IO.Path]::GetFullPath($Expanded)
}

function ConvertTo-AircdBool {
    param($Value, [bool]$Default = $false)
    if ($null -eq $Value) { return $Default }
    if ($Value -is [bool]) { return $Value }
    $Text = ([string]$Value).Trim().ToLowerInvariant()
    switch ($Text) {
        { $_ -in @('1', 'true', 'yes', 'on') } { return $true }
        { $_ -in @('0', 'false', 'no', 'off', '') } { return $false }
        default { throw "Invalid boolean config value '$Value'." }
    }
}

function Normalize-AircdConfigKey {
    param([string]$Key)
    $Key.Trim().ToLowerInvariant().Replace('-', '_')
}

function ConvertFrom-AircdTomlValue {
    param([string]$Raw)
    $Trimmed = $Raw.Trim()
    if ($Trimmed.Length -ge 2 -and $Trimmed.StartsWith('"') -and $Trimmed.EndsWith('"')) {
        $Inner = $Trimmed.Substring(1, $Trimmed.Length - 2)
        $Sb = New-Object System.Text.StringBuilder
        for ($i = 0; $i -lt $Inner.Length; $i++) {
            $Ch = $Inner[$i]
            if ($Ch -eq '\' -and ($i + 1) -lt $Inner.Length) {
                $Next = $Inner[++$i]
                switch ($Next) {
                    '\' { $null = $Sb.Append('\') }
                    '"' { $null = $Sb.Append('"') }
                    'n' { $null = $Sb.Append("`n") }
                    'r' { $null = $Sb.Append("`r") }
                    't' { $null = $Sb.Append("`t") }
                    default { $null = $Sb.Append($Next) }
                }
            } else {
                $null = $Sb.Append($Ch)
            }
        }
        return $Sb.ToString()
    }
    if ($Trimmed.Length -ge 2 -and $Trimmed.StartsWith("'") -and $Trimmed.EndsWith("'")) {
        return $Trimmed.Substring(1, $Trimmed.Length - 2)
    }
    if ($Trimmed -ceq 'true') { return $true }
    if ($Trimmed -ceq 'false') { return $false }
    if ($Trimmed -match '^-?[0-9]+$') { return [int64]$Trimmed }
    return $Trimmed
}

function Remove-AircdTomlComment {
    param([string]$Line)
    $InDouble = $false
    $InSingle = $false
    $Escaped = $false
    for ($i = 0; $i -lt $Line.Length; $i++) {
        $Ch = $Line[$i]
        if ($Escaped) {
            $Escaped = $false
            continue
        }
        if ($InDouble) {
            if ($Ch -eq '\') { $Escaped = $true }
            elseif ($Ch -eq '"') { $InDouble = $false }
            continue
        }
        if ($InSingle) {
            if ($Ch -eq "'") { $InSingle = $false }
            continue
        }
        if ($Ch -eq '"') { $InDouble = $true; continue }
        if ($Ch -eq "'") { $InSingle = $true; continue }
        if ($Ch -eq '#') { return $Line.Substring(0, $i) }
    }
    return $Line
}

function ConvertFrom-AircdToml {
    param([string]$Text)
    $Root = @{}
    $CurrentSection = $Root

    foreach ($RawLine in ($Text -split "`r?`n")) {
        $Clean = (Remove-AircdTomlComment -Line $RawLine).Trim()
        if ([string]::IsNullOrWhiteSpace($Clean)) { continue }

        if ($Clean.StartsWith('[') -and $Clean.EndsWith(']')) {
            $SectionName = Normalize-AircdConfigKey ($Clean.Substring(1, $Clean.Length - 2))
            if (-not $Root.ContainsKey($SectionName) -or -not ($Root[$SectionName] -is [hashtable])) {
                $Root[$SectionName] = @{}
            }
            $CurrentSection = $Root[$SectionName]
            continue
        }

        $EqIndex = $Clean.IndexOf('=')
        if ($EqIndex -le 0) { continue }
        $KeyRaw = $Clean.Substring(0, $EqIndex).Trim().Trim('"', "'")
        $ValRaw = $Clean.Substring($EqIndex + 1).Trim()
        $NormKey = Normalize-AircdConfigKey $KeyRaw
        $CurrentSection[$NormKey] = ConvertFrom-AircdTomlValue -Raw $ValRaw
    }

    return $Root
}

function ConvertTo-AircdNormalizedHashtable {
    param($InputObject)
    $Table = @{}
    if ($null -eq $InputObject) { return $Table }
    if ($InputObject -is [System.Collections.IDictionary]) {
        foreach ($Key in $InputObject.Keys) {
            $NormKey = Normalize-AircdConfigKey ([string]$Key)
            $Val = $InputObject[$Key]
            if ($Val -is [System.Collections.IDictionary] -or $Val -is [pscustomobject]) {
                $Table[$NormKey] = ConvertTo-AircdNormalizedHashtable -InputObject $Val
            } else {
                $Table[$NormKey] = $Val
            }
        }
        return $Table
    }
    if ($InputObject -is [pscustomobject]) {
        foreach ($Prop in $InputObject.PSObject.Properties) {
            $NormKey = Normalize-AircdConfigKey $Prop.Name
            $Val = $Prop.Value
            if ($Val -is [System.Collections.IDictionary] -or $Val -is [pscustomobject]) {
                $Table[$NormKey] = ConvertTo-AircdNormalizedHashtable -InputObject $Val
            } else {
                $Table[$NormKey] = $Val
            }
        }
        return $Table
    }
    return $Table
}

function Find-AircdConfigFile {
    param(
        [string]$RepoRoot = '',
        [string]$ExplicitPath = ''
    )

    if (-not [string]::IsNullOrWhiteSpace($ExplicitPath)) {
        $Full = Resolve-AircdConfigPathValue -Value $ExplicitPath
        if (-not (Test-Path -LiteralPath $Full -PathType Leaf)) {
            throw "Config file not found: $Full"
        }
        return $Full
    }

    if (-not [string]::IsNullOrWhiteSpace($env:AIRCD_WINDOWS_CONFIG)) {
        $Full = Resolve-AircdConfigPathValue -Value $env:AIRCD_WINDOWS_CONFIG
        if (-not (Test-Path -LiteralPath $Full -PathType Leaf)) {
            throw "AIRCD_WINDOWS_CONFIG file not found: $Full"
        }
        return $Full
    }

    if (-not [string]::IsNullOrWhiteSpace($env:AIRCD_CONFIG)) {
        $Full = Resolve-AircdConfigPathValue -Value $env:AIRCD_CONFIG
        if (-not (Test-Path -LiteralPath $Full -PathType Leaf)) {
            throw "AIRCD_CONFIG file not found: $Full"
        }
        return $Full
    }

    $Candidates = New-Object System.Collections.Generic.List[string]
    if (-not [string]::IsNullOrWhiteSpace($RepoRoot)) {
        $Candidates.Add((Join-Path $RepoRoot 'aircd-windows.json'))
        $Candidates.Add((Join-Path $RepoRoot 'aircd.toml'))
    }
    $ConfigDir = Get-AircdDefaultConfigDir -RepoRoot $RepoRoot
    $Candidates.Add((Join-Path $ConfigDir 'aircd-windows.json'))
    $Candidates.Add((Join-Path $ConfigDir 'aircd.json'))
    $Candidates.Add((Join-Path $ConfigDir 'aircd.toml'))

    foreach ($Candidate in $Candidates) {
        if (Test-Path -LiteralPath $Candidate -PathType Leaf) {
            return [IO.Path]::GetFullPath($Candidate)
        }
    }
    return $null
}

function Read-AircdConfigFile {
    param([string]$Path)
    if ([string]::IsNullOrWhiteSpace($Path)) { return @{} }
    $Raw = Get-Content -LiteralPath $Path -Raw -Encoding UTF8
    if ([string]::IsNullOrWhiteSpace($Raw)) { return @{} }

    $Ext = [IO.Path]::GetExtension($Path).ToLowerInvariant()
    if ($Ext -eq '.toml') {
        return ConvertFrom-AircdToml -Text $Raw
    }
    $Parsed = $Raw | ConvertFrom-Json
    return ConvertTo-AircdNormalizedHashtable -InputObject $Parsed
}

function Get-AircdFileSetting {
    param(
        [hashtable]$ConfigData,
        [string[]]$Sections,
        [string[]]$Keys
    )
    if ($null -eq $ConfigData -or $ConfigData.Count -eq 0) {
        return [pscustomobject]@{ Found = $false; Value = $null }
    }

    foreach ($Section in $Sections) {
        $Target = if ([string]::IsNullOrEmpty($Section)) {
            $ConfigData
        } elseif ($ConfigData.ContainsKey($Section) -and ($ConfigData[$Section] -is [hashtable])) {
            $ConfigData[$Section]
        } else {
            $null
        }
        if ($null -eq $Target) { continue }
        foreach ($Key in $Keys) {
            if ($Target.ContainsKey($Key)) {
                return [pscustomobject]@{ Found = $true; Value = $Target[$Key] }
            }
        }
    }
    return [pscustomobject]@{ Found = $false; Value = $null }
}

function Get-AircdEnvValue {
    param([string[]]$Names)
    foreach ($Name in $Names) {
        $Item = Get-Item -LiteralPath "Env:\$Name" -ErrorAction SilentlyContinue
        if ($null -ne $Item -and -not [string]::IsNullOrWhiteSpace([string]$Item.Value)) {
            return [pscustomobject]@{ Found = $true; Value = ([string]$Item.Value).Trim() }
        }
    }
    return [pscustomobject]@{ Found = $false; Value = $null }
}

function Resolve-AircdWindowsConfig {
    param(
        [string]$RepoRoot = '',
        [string]$ConfigPath = '',
        [hashtable]$Overrides = @{}
    )

    if ($null -eq $Overrides) { $Overrides = @{} }
    $ResolvedRepoRoot = if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
        [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
    } else {
        [IO.Path]::GetFullPath($RepoRoot)
    }

    $ResolvedConfigPath = Find-AircdConfigFile -RepoRoot $ResolvedRepoRoot -ExplicitPath $ConfigPath
    $ConfigDir = if ($ResolvedConfigPath) { Split-Path -Parent $ResolvedConfigPath } else { '' }
    $FileData = if ($ResolvedConfigPath) { Read-AircdConfigFile -Path $ResolvedConfigPath } else { @{} }

    $ResolveSetting = {
        param(
            [string]$OverrideKey,
            [string[]]$EnvNames,
            [string[]]$Sections,
            [string[]]$FileKeys,
            $DefaultValue,
            [switch]$IsPath,
            [switch]$IsBool
        )

        $Source = 'default'
        $Raw = $DefaultValue
        if ($Overrides.ContainsKey($OverrideKey)) {
            $Source = 'cli'
            $Raw = $Overrides[$OverrideKey]
        } else {
            $EnvHit = Get-AircdEnvValue -Names $EnvNames
            if ($EnvHit.Found) {
                $Source = 'env'
                $Raw = $EnvHit.Value
            } else {
                $FileHit = Get-AircdFileSetting -ConfigData $FileData -Sections $Sections -Keys $FileKeys
                if ($FileHit.Found -and $null -ne $FileHit.Value) {
                    $Source = 'file'
                    $Raw = $FileHit.Value
                }
            }
        }

        if ($IsBool) {
            return [pscustomobject]@{
                Source = $Source
                Value  = ConvertTo-AircdBool -Value $Raw -Default ([bool]$DefaultValue)
            }
        }
        $Text = if ($null -eq $Raw) { '' } else { [string]$Raw }
        if ($IsPath) {
            $Base = if ($Source -eq 'file') { $ConfigDir } else { '' }
            return [pscustomobject]@{
                Source = $Source
                Value  = Resolve-AircdConfigPathValue -Value $Text -BaseDir $Base
            }
        }
        return [pscustomobject]@{
            Source = $Source
            Value  = (Expand-AircdConfigString -Value $Text)
        }
    }

    $DefaultDataDir = Get-AircdDefaultDataDir -RepoRoot $ResolvedRepoRoot
    $DefaultDbPath = Join-Path $DefaultDataDir 'aircd.sqlite3'
    $DefaultStartup = [Environment]::GetFolderPath('Startup')

    $BindRes = & $ResolveSetting 'Bind' @('AIRCD_BIND') @('server', '') @('bind') '127.0.0.1:6667'
    $HttpBindRes = & $ResolveSetting 'HttpBind' @('AIRCD_HTTP_BIND') @('server', '') @('http_bind') '127.0.0.1:6680'
    $DbPathRes = & $ResolveSetting 'DbPath' @('AIRCD_DB', 'AIRCD_DB_PATH') @('server', '') @('db_path', 'db') $DefaultDbPath -IsPath
    $OpenRegRes = & $ResolveSetting 'OpenRegistration' @('AIRCD_OPEN_REGISTRATION') @('server', '') @('open_registration') $false -IsBool
    $RegKeyRes = & $ResolveSetting 'RegisterKey' @('AIRCD_REGISTER_KEY') @('server', '') @('register_key') ''

    # Never pair an explicit CLI/env TLS half with a file-sourced other half.
    $TlsCliOverride = $Overrides.ContainsKey('TlsCert') -or $Overrides.ContainsKey('TlsKey')
    $TlsEnvOverride = (Get-AircdEnvValue -Names @('AIRCD_TLS_CERT')).Found -or (Get-AircdEnvValue -Names @('AIRCD_TLS_KEY')).Found
    $TlsCertRes = if ($Overrides.ContainsKey('TlsCert')) {
        [pscustomobject]@{ Source = 'cli'; Value = Resolve-AircdConfigPathValue -Value ([string]$Overrides['TlsCert']) }
    } elseif ($TlsCliOverride) {
        [pscustomobject]@{ Source = 'default'; Value = '' }
    } else {
        $EnvCert = Get-AircdEnvValue -Names @('AIRCD_TLS_CERT')
        if ($EnvCert.Found) {
            [pscustomobject]@{ Source = 'env'; Value = Resolve-AircdConfigPathValue -Value $EnvCert.Value }
        } elseif ($TlsEnvOverride) {
            [pscustomobject]@{ Source = 'default'; Value = '' }
        } else {
            & $ResolveSetting 'TlsCert' @() @('server', '') @('tls_cert') '' -IsPath
        }
    }
    $TlsKeyRes = if ($Overrides.ContainsKey('TlsKey')) {
        [pscustomobject]@{ Source = 'cli'; Value = Resolve-AircdConfigPathValue -Value ([string]$Overrides['TlsKey']) }
    } elseif ($TlsCliOverride) {
        [pscustomobject]@{ Source = 'default'; Value = '' }
    } else {
        $EnvKey = Get-AircdEnvValue -Names @('AIRCD_TLS_KEY')
        if ($EnvKey.Found) {
            [pscustomobject]@{ Source = 'env'; Value = Resolve-AircdConfigPathValue -Value $EnvKey.Value }
        } elseif ($TlsEnvOverride) {
            [pscustomobject]@{ Source = 'default'; Value = '' }
        } else {
            & $ResolveSetting 'TlsKey' @() @('server', '') @('tls_key') '' -IsPath
        }
    }

    $TlsBindRes = & $ResolveSetting 'TlsBind' @('AIRCD_TLS_BIND') @('server', '') @('tls_bind') '127.0.0.1:6697'
    $TlsClientCaRes = & $ResolveSetting 'TlsClientCa' @('AIRCD_TLS_CLIENT_CA') @('server', '') @('tls_client_ca') '' -IsPath
    $TlsClientAuthRes = & $ResolveSetting 'TlsClientAuth' @('AIRCD_TLS_CLIENT_AUTH') @('server', '') @('tls_client_auth') ''
    $HttpTlsBindRes = & $ResolveSetting 'HttpTlsBind' @('AIRCD_HTTP_TLS_BIND') @('server', '') @('http_tls_bind') ''
    $TlsOnlyRes = & $ResolveSetting 'TlsOnly' @('AIRCD_TLS_ONLY') @('server', '') @('tls_only') $false -IsBool
    $WinEventLogRes = & $ResolveSetting 'WindowsEventLog' @('AIRCD_WINDOWS_EVENT_LOG') @('server', '') @('windows_event_log') $false -IsBool
    $WinEventSourceRes = & $ResolveSetting 'WindowsEventLogSource' @('AIRCD_WINDOWS_EVENT_LOG_SOURCE') @('server', '') @('windows_event_log_source') ''
    $SyslogTargetRes = & $ResolveSetting 'SyslogTarget' @('AIRCD_SYSLOG_TARGET') @('server', '') @('syslog_target') ''
    $SyslogFormatRes = & $ResolveSetting 'SyslogFormat' @('AIRCD_SYSLOG_FORMAT') @('server', '') @('syslog_format') ''
    $SyslogFacilityRes = & $ResolveSetting 'SyslogFacility' @('AIRCD_SYSLOG_FACILITY') @('server', '') @('syslog_facility') ''
    $BackupEnabledRes = & $ResolveSetting 'BackupEnabled' @('AIRCD_BACKUP_ENABLED') @('server', '') @('backup_enabled') $true -IsBool
    $BackupDirRes = & $ResolveSetting 'BackupDir' @('AIRCD_BACKUP_DIR') @('server', '') @('backup_dir') '' -IsPath
    $BackupIntervalRes = & $ResolveSetting 'BackupIntervalSecs' @('AIRCD_BACKUP_INTERVAL_SECS') @('server', '') @('backup_interval_secs') ''
    $BackupRetentionRes = & $ResolveSetting 'BackupRetention' @('AIRCD_BACKUP_RETENTION') @('server', '') @('backup_retention') ''
    $BackupOnStartupRes = & $ResolveSetting 'BackupOnStartup' @('AIRCD_BACKUP_ON_STARTUP') @('server', '') @('backup_on_startup') $false -IsBool
    $ExeRes = & $ResolveSetting 'ExecutablePath' @('AIRCD_EXECUTABLE') @('server', 'service', 'tray', '') @('executable_path', 'executable') '' -IsPath

    $NameRes = & $ResolveSetting 'Name' @('AIRCD_SERVICE_NAME') @('service', '') @('name', 'service_name') 'CAMS-Aircd'
    $TaskPathRes = & $ResolveSetting 'TaskPath' @('AIRCD_TASK_PATH') @('service', '') @('task_path') '\'
    $ModeRes = & $ResolveSetting 'Mode' @('AIRCD_SERVICE_MODE') @('service', '') @('mode') 'Auto'
    $TriggerRes = & $ResolveSetting 'Trigger' @('AIRCD_SERVICE_TRIGGER') @('service', '') @('trigger') 'Auto'

    $ProfileRes = & $ResolveSetting 'Profile' @('AIRCD_PROFILE') @('tray', '') @('profile') ''
    $StartupRes = & $ResolveSetting 'StartupDirectory' @('AIRCD_STARTUP_DIR') @('tray', '') @('startup_directory') $DefaultStartup -IsPath

    [pscustomobject]@{
        RepoRoot              = $ResolvedRepoRoot
        ConfigPath            = $ResolvedConfigPath
        DataDir               = $DefaultDataDir
        Bind                  = $BindRes.Value
        HttpBind              = $HttpBindRes.Value
        DbPath                = $DbPathRes.Value
        OpenRegistration      = $OpenRegRes.Value
        RegisterKey           = $RegKeyRes.Value
        TlsCert               = $TlsCertRes.Value
        TlsKey                = $TlsKeyRes.Value
        TlsBind               = $TlsBindRes.Value
        TlsClientCa           = $TlsClientCaRes.Value
        TlsClientAuth         = $TlsClientAuthRes.Value
        HttpTlsBind           = $HttpTlsBindRes.Value
        TlsOnly               = $TlsOnlyRes.Value
        WindowsEventLog       = $WinEventLogRes.Value
        WindowsEventLogSource = $WinEventSourceRes.Value
        SyslogTarget          = $SyslogTargetRes.Value
        SyslogFormat          = $SyslogFormatRes.Value
        SyslogFacility        = $SyslogFacilityRes.Value
        BackupEnabled         = $BackupEnabledRes.Value
        BackupDir             = $BackupDirRes.Value
        BackupIntervalSecs    = $BackupIntervalRes.Value
        BackupRetention       = $BackupRetentionRes.Value
        BackupOnStartup       = $BackupOnStartupRes.Value
        ExecutablePath        = $ExeRes.Value
        Name                  = $NameRes.Value
        TaskPath              = $TaskPathRes.Value
        Mode                  = $ModeRes.Value
        Trigger               = $TriggerRes.Value
        Profile               = $ProfileRes.Value
        StartupDirectory      = $StartupRes.Value
    }
}

Export-ModuleMember -Function `
    Get-AircdDefaultDataDir, `
    Get-AircdDefaultConfigDir, `
    Expand-AircdConfigString, `
    Resolve-AircdConfigPathValue, `
    Find-AircdConfigFile, `
    Read-AircdConfigFile, `
    Resolve-AircdWindowsConfig
