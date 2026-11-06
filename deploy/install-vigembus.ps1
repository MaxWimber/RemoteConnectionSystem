<#
.SYNOPSIS
  Phase 5 (D-024): installs the ViGEmBus 1.22.0 driver for virtual Xbox 360 controllers. Run by the user.
.DESCRIPTION
  Downloads the installer from the ViGEmBus release on GitHub, checks its Authenticode signature (Nefarius) and
  prints its SHA-256, runs it (the installer asks for administrator rights itself) and checks that the ViGEmBus
  service runs. Writes report-vigembus.json. Nothing is installed if the signature is not valid.

    powershell -ExecutionPolicy Bypass -File deploy\install-vigembus.ps1
#>
param(
    [string]$OutDir = (Join-Path (Split-Path -Parent $PSScriptRoot) 'out\phase-05\vigembus')
)

$ErrorActionPreference = 'Stop'
$version = '1.22.0'
$url = "https://github.com/nefarius/ViGEmBus/releases/download/v$version/ViGEmBus_${version}_x64_x86_arm64.exe"
$report = [ordered]@{ version = $version; url = $url; sha256 = $null; signer = $null; signature = $null;
    exit_code = $null; service = $null; error = $null }

try {
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $installer = Join-Path $OutDir "ViGEmBus_$version.exe"
    Write-Host "Скачивание $url"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $url -OutFile $installer -UseBasicParsing
    $report.sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $installer).Hash
    $sig = Get-AuthenticodeSignature -LiteralPath $installer
    $report.signature = "$($sig.Status)"
    $report.signer = $sig.SignerCertificate.Subject
    Write-Host "SHA-256: $($report.sha256)"
    Write-Host "Подпись: $($sig.Status), $($report.signer)"
    if ($sig.Status -ne 'Valid' -or $report.signer -notmatch 'Nefarius') {
        throw 'Подпись установщика не подтверждена, установка отменена.'
    }
    Write-Host 'Запуск установщика: подтвердите запрос UAC и пройдите шаги установки.' -ForegroundColor Green
    $p = Start-Process -FilePath $installer -Wait -PassThru
    $report.exit_code = $p.ExitCode
    $service = Get-Service -Name ViGEmBus -ErrorAction SilentlyContinue
    $report.service = if ($service) { "$($service.Status)" } else { 'нет службы' }
    if (-not $service -or $service.Status -ne 'Running') {
        throw "Служба ViGEmBus не запущена (код установщика $($p.ExitCode))."
    }
    Write-Host 'ViGEmBus установлен и работает.' -ForegroundColor Green
} catch {
    $report.error = $_.Exception.Message
    Write-Host "Ошибка: $($report.error)" -ForegroundColor Red
} finally {
    if (Test-Path -LiteralPath $OutDir) {
        $report | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath (Join-Path $OutDir 'report-vigembus.json') -Encoding UTF8
    }
    $report | ConvertTo-Json -Depth 3 | Write-Host
}
