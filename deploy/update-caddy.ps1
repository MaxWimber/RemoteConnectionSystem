<#
.SYNOPSIS
  Phase 3 (P3-9): Caddy proxies rcs-web, and the phase 2 LAN firewall rules are removed. Run as administrator.
.DESCRIPTION
  Copies deploy\Caddyfile to %ProgramData%\RCS, validates it, restarts the rcs-caddy service and checks the site
  through Caddy on this machine. rcs-web must already run on 127.0.0.1:8080 with secrets set up. The report goes to
  OutDir\report-caddy-proxy.json. Safe to run again.
#>
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$Domain = 'maxwimber.ru',
    [switch]$KeepLanRules
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$serviceName = 'rcs-caddy'
$httpsPort = 8444
$dataDir = Join-Path $env:ProgramData 'RCS'
$configPath = Join-Path $dataDir 'Caddyfile'
$caddyExe = Join-Path $dataDir 'bin\caddy.exe'
$site = "https://${Domain}:$httpsPort/RemoteConnectionSystem"

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Нужны права администратора: откройте PowerShell от имени администратора.'
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$report = [ordered]@{
    config_valid = $false; service_running = $false; page_status = $null; hsts = $null; csp = $null
    api_without_session = $null; signal_without_session = $null; http_redirect = $null; lan_rules_left = $null; error = $null
}

# Everything goes to Caddy on this machine, as a browser from outside would see it.
function Invoke-Curl([string[]]$Arguments) {
    $output = & curl.exe -s --max-time 15 --resolve "${Domain}:${httpsPort}:127.0.0.1" --resolve "${Domain}:80:127.0.0.1" @Arguments
    return "$output".Trim()
}

try {
    $listening = Get-NetTCPConnection -State Listen -LocalPort 8080 -ErrorAction SilentlyContinue
    if (-not $listening -or ($listening | Where-Object { $_.LocalAddress -ne '127.0.0.1' })) {
        throw 'rcs-web должен слушать только 127.0.0.1:8080. Запустите его перед этим скриптом.'
    }

    $backup = "$configPath.phase0"
    if (-not (Test-Path $backup)) { Copy-Item -LiteralPath $configPath -Destination $backup }
    Copy-Item -LiteralPath (Join-Path $root 'deploy\Caddyfile') -Destination $configPath -Force
    & $caddyExe validate --config $configPath --adapter caddyfile 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Copy-Item -LiteralPath $backup -Destination $configPath -Force
        throw 'Caddyfile не прошёл проверку, прежняя настройка возвращена.'
    }
    $report.config_valid = $true

    Restart-Service -Name $serviceName
    Start-Sleep -Seconds 3
    $report.service_running = (Get-Service -Name $serviceName).Status -eq 'Running'

    $headers = Invoke-Curl @('-D', '-', '-o', 'NUL', "$site/")
    $report.page_status = if ($headers -match 'HTTP/\S+\s+(\d+)') { $Matches[1] } else { $null }
    $report.hsts = if ($headers -match '(?im)^strict-transport-security:\s*(.+)$') { $Matches[1].Trim() } else { $null }
    $report.csp = if ($headers -match '(?im)^content-security-policy:\s*(.+)$') { $Matches[1].Trim() } else { $null }
    $report.api_without_session = Invoke-Curl @('-o', 'NUL', '-w', '%{http_code}', "$site/api/status")
    $report.signal_without_session = Invoke-Curl @('-o', 'NUL', '-w', '%{http_code}', '-H', 'Connection: Upgrade', '-H', 'Upgrade: websocket', '-H', 'Sec-WebSocket-Version: 13', '-H', 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==', '-H', "Origin: https://${Domain}:$httpsPort", "$site/api/signal")
    $report.http_redirect = Invoke-Curl @('-o', 'NUL', '-w', '%{redirect_url}', "http://$Domain/RemoteConnectionSystem/")

    if (-not $KeepLanRules) {
        & (Join-Path $root 'scripts\firewall-lan.ps1') -Remove | Out-Null
    }
    $report.lan_rules_left = @(Get-NetFirewallRule -Group 'RCS phase 2 LAN test' -ErrorAction SilentlyContinue).Count
} catch {
    $report.error = $_.Exception.Message
    Write-Host "Ошибка: $($report.error)" -ForegroundColor Red
} finally {
    $report | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $OutDir 'report-caddy-proxy.json') -Encoding UTF8
    $report | Format-List | Out-String | Write-Host
}
