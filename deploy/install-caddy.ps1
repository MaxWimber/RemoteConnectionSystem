<#
.SYNOPSIS
  Installs Caddy as the rcs-caddy service. Run as administrator on the server only.
.DESCRIPTION
  Downloads the pinned official release, verifies its SHA-256, runs the service as LocalService
  and adds one firewall rule for caddy.exe on TCP 80 and 8444. Safe to run again.
#>
param(
    [Parameter(Mandatory = $true)][string]$PackageDir,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$Domain = 'maxwimber.ru',
    [switch]$SkipDnsCheck
)

$ErrorActionPreference = 'Stop'
$caddyVersion = '2.11.7'
$caddyUrl = "https://github.com/caddyserver/caddy/releases/download/v$caddyVersion/caddy_${caddyVersion}_windows_amd64.zip"
$caddySha256 = '0a1edc0b799512051c57071ce0e798d3f2cf67dc98171366f1d2b326072e3b06'
$serviceName = 'rcs-caddy'
$ruleName = 'RCS Caddy HTTP and HTTPS'
$httpsPort = 8444
$siteUrl = "https://${Domain}:$httpsPort/RemoteConnectionSystem"

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Нужны права администратора: откройте PowerShell от имени администратора.'
}

$dataDir = Join-Path $env:ProgramData 'RCS'
$binDir = Join-Path $dataDir 'bin'
$caddyData = Join-Path $dataDir 'caddy'
$logDir = Join-Path $dataDir 'logs'
$wwwDir = Join-Path $dataDir 'www'
$configPath = Join-Path $dataDir 'Caddyfile'
$caddyExe = Join-Path $binDir 'caddy.exe'

[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
New-Item -ItemType Directory -Force -Path $OutDir, $binDir, $caddyData, $logDir, $wwwDir | Out-Null

$report = [ordered]@{ version = $caddyVersion; dns_warning = $null; service_running = $false; service_account = $null; http_status = $null; http_redirect = $null; https_status = $null; subpath_status = $null; certificate_valid = $false; error = $null }

function Save-Report {
    $report | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $OutDir 'report-caddy.json') -Encoding UTF8
}

function Get-Response([string]$Url, [string]$Format) {
    $output = & curl.exe -s -o NUL -w $Format --max-time 15 --resolve "${Domain}:${httpsPort}:127.0.0.1" --resolve "${Domain}:80:127.0.0.1" $Url
    return "$output".Trim()
}

function Get-StatusCode([string]$Url) {
    return Get-Response $Url '%{http_code}'
}

try {
    if (-not $SkipDnsCheck) {
        # The local resolver can cache an old answer, so ask a public resolver what Let's Encrypt will see.
        $answer = Invoke-RestMethod -Uri "https://cloudflare-dns.com/dns-query?name=$Domain&type=A" -Headers @{ Accept = 'application/dns-json' } -TimeoutSec 10
        $records = @($answer.Answer | Where-Object { $_.type -eq 1 } | ForEach-Object { $_.data })
        $publicIp = "$(Invoke-RestMethod -Uri 'https://api.ipify.org' -TimeoutSec 10)".Trim()
        # Outgoing traffic may use another address than the one that receives connections, so this is only a warning.
        if ($records -notcontains $publicIp) {
            $report.dns_warning = "A-запись домена ($($records -join ', ')) отличается от исходящего адреса сервера."
            Write-Host "Предупреждение: $($report.dns_warning) Исходящий адрес: $publicIp." -ForegroundColor Yellow
        }
    }

    if (Get-Service -Name $serviceName -ErrorAction SilentlyContinue) {
        Stop-Service -Name $serviceName -Force
    }

    $busy = @(Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue | Where-Object { $_.LocalPort -in 80, $httpsPort })
    if ($busy.Count -gt 0) {
        $owners = ($busy | ForEach-Object { "$($_.LocalPort): $((Get-Process -Id $_.OwningProcess -ErrorAction SilentlyContinue).ProcessName)" }) -join ', '
        throw "Порты заняты другими программами: $owners"
    }

    if (-not (Test-Path $caddyExe) -or "$(& $caddyExe version)" -notlike "v$caddyVersion*") {
        $temp = Join-Path $env:TEMP ("rcs-caddy-" + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $temp | Out-Null
        try {
            $zip = Join-Path $temp 'caddy.zip'
            Write-Host "Загрузка Caddy $caddyVersion..."
            Invoke-WebRequest -Uri $caddyUrl -OutFile $zip -UseBasicParsing
            $actual = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($actual -ne $caddySha256) { throw "Контрольная сумма Caddy не совпала: $actual" }
            Expand-Archive -LiteralPath $zip -DestinationPath $temp
            Copy-Item -LiteralPath (Join-Path $temp 'caddy.exe') -Destination $caddyExe -Force
        } finally {
            Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    Copy-Item -LiteralPath (Join-Path $PackageDir 'deploy\Caddyfile') -Destination $configPath -Force
    Copy-Item -Path (Join-Path $PackageDir 'www\*') -Destination $wwwDir -Recurse -Force

    & $caddyExe validate --config $configPath --adapter caddyfile
    if ($LASTEXITCODE -ne 0) { throw 'Caddyfile не прошёл проверку' }

    & icacls.exe $caddyData /grant '*S-1-5-19:(OI)(CI)M' /T /Q | Out-Null
    & icacls.exe $logDir /grant '*S-1-5-19:(OI)(CI)M' /T /Q | Out-Null

    if (-not (Get-Service -Name $serviceName -ErrorAction SilentlyContinue)) {
        $binPath = """$caddyExe"" run --config ""$configPath"" --adapter caddyfile"
        $account = New-Object System.Management.Automation.PSCredential('NT AUTHORITY\LocalService', (New-Object System.Security.SecureString))
        New-Service -Name $serviceName -BinaryPathName $binPath -DisplayName 'RCS Caddy' -StartupType Automatic -Credential $account | Out-Null
        & sc.exe failure $serviceName reset= 86400 actions= restart/5000/restart/30000/restart/60000 | Out-Null
        Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Services\$serviceName" -Name Environment -Type MultiString -Value @(
            "XDG_CONFIG_HOME=$caddyData\config",
            "XDG_DATA_HOME=$caddyData\data"
        )
    }

    $rule = Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
    if ($rule) {
        $rule | Get-NetFirewallPortFilter | Set-NetFirewallPortFilter -Protocol TCP -LocalPort 80, $httpsPort
    } else {
        New-NetFirewallRule -DisplayName $ruleName -Direction Inbound -Action Allow -Protocol TCP -LocalPort 80, $httpsPort -Program $caddyExe -Profile Any | Out-Null
    }

    Start-Service -Name $serviceName
    $report.service_running = (Get-Service -Name $serviceName).Status -eq 'Running'
    $report.service_account = (Get-CimInstance Win32_Service -Filter "Name='$serviceName'").StartName

    Write-Host 'Ожидание сертификата, до 2 минут...'
    $deadline = (Get-Date).AddMinutes(2)
    while ((Get-Date) -lt $deadline) {
        $report.https_status = Get-StatusCode "$siteUrl/"
        if ($report.https_status -eq '200') { break }
        Start-Sleep -Seconds 5
    }
    $report.certificate_valid = $report.https_status -eq '200'
    $report.http_status = Get-StatusCode "http://$Domain/RemoteConnectionSystem/"
    $report.http_redirect = Get-Response "http://$Domain/RemoteConnectionSystem/" '%{redirect_url}'
    $report.subpath_status = Get-StatusCode $siteUrl
} catch {
    $report.error = $_.Exception.Message
    Write-Host "Ошибка: $($report.error)" -ForegroundColor Red
} finally {
    Save-Report
}

if ($report.certificate_valid) {
    Write-Host "Готово. Откройте $siteUrl/ с устройства вне домашней сети." -ForegroundColor Green
} else {
    Write-Host 'Заглушка по HTTPS пока недоступна. Соберите логи и верните архив.' -ForegroundColor Yellow
}
