<#
.SYNOPSIS
  Captures packet headers of the site (TCP 8444) and the media (UDP 8443) with pktmon. Run as administrator.
.DESCRIPTION
  Shows who ends a connection (FIN or RST, from which side) and whether new connections reach the server. Only the first
  128 bytes of each packet are kept: TLS and SRTP payloads are encrypted anyway. Stops on Enter or after -Seconds,
  removes its pktmon filters and writes capture.etl, capture.txt, capture.pcapng and report-net-capture.json.

    powershell -ExecutionPolicy Bypass -File scripts\net-capture.ps1 -OutDir out\phase-03\net-capture
#>
param(
    [int]$Seconds = 600,
    [string]$OutDir = (Join-Path (Split-Path -Parent $PSScriptRoot) 'out\phase-03\net-capture')
)

$ErrorActionPreference = 'Stop'
$report = [ordered]@{ started = $null; stopped = $null; seconds = 0; files = @(); error = $null }

function Invoke-Pktmon([string[]]$Arguments) {
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & pktmon @Arguments 2>&1 | ForEach-Object { "$_" } } finally { $ErrorActionPreference = $old }
}

try {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Нужны права администратора: откройте PowerShell от имени администратора.'
    }
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $etl = Join-Path $OutDir 'capture.etl'
    Invoke-Pktmon @('stop') | Out-Null
    Invoke-Pktmon @('filter', 'remove') | Out-Null
    Invoke-Pktmon @('filter', 'add', 'RCS-site', '-t', 'TCP', '-p', '8444') | Out-Null
    Invoke-Pktmon @('filter', 'add', 'RCS-media', '-t', 'UDP', '-p', '8443') | Out-Null
    Invoke-Pktmon @('start', '--capture', '--comp', 'nics', '--pkt-size', '128', '--file-name', $etl, '--file-size', '512') |
        Write-Host
    $report.started = (Get-Date).ToUniversalTime().ToString('o')
    Write-Host ''
    Write-Host "Захват идёт. Откройте сайт на телефоне через мобильный интернет, войдите и смотрите поток 5 минут." -ForegroundColor Green
    Write-Host "Нажмите Enter, чтобы остановить раньше (не больше $Seconds с)." -ForegroundColor Green
    $begin = Get-Date
    while (((Get-Date) - $begin).TotalSeconds -lt $Seconds) {
        if ([Console]::KeyAvailable -and [Console]::ReadKey($true).Key -eq 'Enter') { break }
        Start-Sleep -Milliseconds 200
    }
    $report.seconds = [int]((Get-Date) - $begin).TotalSeconds
} catch {
    $report.error = $_.Exception.Message
    Write-Host "Ошибка: $($report.error)" -ForegroundColor Red
} finally {
    Invoke-Pktmon @('stop') | Write-Host
    Invoke-Pktmon @('filter', 'remove') | Out-Null
    $report.stopped = (Get-Date).ToUniversalTime().ToString('o')
    if ($etl -and (Test-Path -LiteralPath $etl)) {
        Invoke-Pktmon @('etl2txt', $etl, '--out', (Join-Path $OutDir 'capture.txt')) | Out-Null
        Invoke-Pktmon @('etl2pcap', $etl, '--out', (Join-Path $OutDir 'capture.pcapng')) | Out-Null
        $report.files = @(Get-ChildItem -LiteralPath $OutDir -Filter 'capture.*' | ForEach-Object { "$($_.Name) $($_.Length)" })
    }
    if ($OutDir -and (Test-Path -LiteralPath $OutDir)) {
        $report | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $OutDir 'report-net-capture.json') -Encoding UTF8
    }
    $report | ConvertTo-Json -Depth 4 | Write-Host
    Write-Host 'Готово, окно можно закрыть.'
}
