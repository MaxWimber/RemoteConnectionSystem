<#
.SYNOPSIS
  Collects reports and logs into a redacted archive. Never reads the secrets directory.
#>
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [Parameter(Mandatory = $true)][string]$Destination,
    [string]$Phase = 'unknown',
    [string]$Build = 'unknown',
    [string]$DataDir = (Join-Path $env:ProgramData 'RCS')
)

$ErrorActionPreference = 'Stop'
$allowed = '.json', '.jsonl', '.txt', '.log', '.csv'

function Test-PublicIPv4([string]$Address) {
    $octets = $Address.Split('.') | ForEach-Object { [int]$_ }
    if ($octets | Where-Object { $_ -gt 255 }) { return $false }
    if ($octets[0] -in 0, 10, 127 -or $octets[0] -ge 224) { return $false }
    if ($octets[0] -eq 169 -and $octets[1] -eq 254) { return $false }
    if ($octets[0] -eq 172 -and $octets[1] -ge 16 -and $octets[1] -le 31) { return $false }
    if ($octets[0] -eq 192 -and $octets[1] -eq 168) { return $false }
    return $true
}

# Running services keep their logs open, so the file must be shared with them while it is read.
function Read-SharedText([string]$Path) {
    $share = [System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete
    $stream = New-Object System.IO.FileStream($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, $share)
    $reader = New-Object System.IO.StreamReader($stream, [System.Text.Encoding]::UTF8)
    try {
        return $reader.ReadToEnd()
    } finally {
        $reader.Dispose()
    }
}

function Protect-Text([string]$Text, [switch]$Addresses) {
    # Header values can hold spaces and several cookies, so they are removed up to the closing quote or the line end.
    $result = [regex]::Replace($Text, '(?i)("?(?:set-cookie|cookie|authorization)"?\s*[:=]\s*\[?\s*"?)[^"\]\r\n]+', '$1<redacted>')
    $result = [regex]::Replace($result, '(?i)("?(?:password|secret|token)"?\s*[:=]\s*"?)[^\s",;\]]+', '$1<redacted>')
    foreach ($name in @($env:COMPUTERNAME, $env:USERNAME, $env:USERDOMAIN) | Where-Object { $_ }) {
        $result = [regex]::Replace($result, [regex]::Escape($name), '<name>', 'IgnoreCase')
    }
    if ($Addresses) {
        $result = [regex]::Replace($result, '\b(?:\d{1,3}\.){3}\d{1,3}\b', {
                param($match)
                if (Test-PublicIPv4 $match.Value) { '<public-ip>' } else { $match.Value }
            })
        $result = [regex]::Replace($result, '\b(?:[0-9a-fA-F]{1,4}:){1,6}:(?:[0-9a-fA-F]{1,4}(?::[0-9a-fA-F]{1,4})*)?', '<ipv6>')
        $result = [regex]::Replace($result, '\b(?:[0-9a-fA-F]{1,4}:){3,7}[0-9a-fA-F]{1,4}\b', '<ipv6>')
    }
    return $result
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("rcs-logs-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path (Join-Path $work 'logs') | Out-Null

try {
    $utf8 = New-Object System.Text.UTF8Encoding($false)

    if (Test-Path $OutDir) {
        Get-ChildItem -Path $OutDir -File | Where-Object { $allowed -contains $_.Extension.ToLowerInvariant() } | ForEach-Object {
            # Generated reports hold version numbers that look like IPv4 addresses and never hold client addresses.
            $text = Protect-Text (Read-SharedText $_.FullName)
            [System.IO.File]::WriteAllText((Join-Path $work $_.Name), $text, $utf8)
        }
    }

    $logDir = Join-Path $DataDir 'logs'
    if (Test-Path $logDir) {
        Get-ChildItem -Path $logDir -File -Recurse |
            Where-Object { $allowed -contains $_.Extension.ToLowerInvariant() -and $_.FullName -notmatch '\\secrets\\' } |
            Sort-Object LastWriteTime -Descending | Select-Object -First 30 | ForEach-Object {
                try {
                    $text = Protect-Text -Text (Read-SharedText $_.FullName) -Addresses
                } catch {
                    # One unreadable log must not cost the rest of the archive.
                    $text = "Log unavailable: $($_.Exception.Message)"
                }
                [System.IO.File]::WriteAllText((Join-Path $work "logs\$($_.Name)"), $text, $utf8)
            }
    }

    try {
        $since = (Get-Date).AddDays(-2)
        $events = Get-WinEvent -FilterHashtable @{ LogName = 'System', 'Application'; Level = 1, 2, 3; StartTime = $since } -MaxEvents 3000 -ErrorAction Stop |
            Where-Object { "$($_.ProviderName) $($_.Message)" -match '(?i)rcs|caddy|amf|amdkmdag|amdwddmg|display|service control manager' } |
            ForEach-Object {
                $message = "$($_.Message)" -replace '\s+', ' '
                if ($message.Length -gt 300) { $message = $message.Substring(0, 300) }
                '{0:o} {1} {2} {3}: {4}' -f $_.TimeCreated, $_.LevelDisplayName, $_.ProviderName, $_.Id, $message
            }
        $text = if ($events) { Protect-Text -Text ($events -join "`n") -Addresses } else { 'No matching events in the last 2 days.' }
        [System.IO.File]::WriteAllText((Join-Path $work 'events.txt'), $text, $utf8)
    } catch {
        [System.IO.File]::WriteAllText((Join-Path $work 'events.txt'), "Events unavailable: $($_.Exception.Message)", $utf8)
    }

    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $archive = Join-Path $Destination "rcs-logs-phase$Phase-$Build-$stamp.zip"
    Compress-Archive -Path (Join-Path $work '*') -DestinationPath $archive
    Write-Host "Архив логов готов: $archive" -ForegroundColor Green
    Write-Output $archive
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
