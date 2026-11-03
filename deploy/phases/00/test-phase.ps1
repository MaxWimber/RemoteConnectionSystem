<#
.SYNOPSIS
  Phase 0 test menu for the server. -Step runs one step without the menu.
#>
param(
    [ValidateSet('menu', 'probe', 'install', 'udp', 'collect', 'rollback')][string]$Step = 'menu',
    [string]$Bind = '0.0.0.0',
    [int]$Seconds = 180
)

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$out = Join-Path $here 'out'
New-Item -ItemType Directory -Force -Path $out | Out-Null
$build = (Get-Content -LiteralPath (Join-Path $here 'build.json') -Raw -Encoding UTF8 | ConvertFrom-Json)

function Test-IsAdmin {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Test-Package {
    $bad = @()
    Get-Content -LiteralPath (Join-Path $here 'SHA256SUMS.txt') -Encoding UTF8 | Where-Object { $_ } | ForEach-Object {
        $parts = $_ -split '  ', 2
        $path = Join-Path $here $parts[1]
        if (-not (Test-Path -LiteralPath $path) -or (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $parts[0]) {
            $bad += $parts[1]
        }
    }
    return $bad
}

function Invoke-Probe {
    $bad = @(Test-Package)
    $binary = Join-Path $here 'bin\rcs-bench.exe'
    $smoke = [ordered]@{ package_ok = $bad.Count -eq 0; damaged_files = $bad; binary_output = $null; binary_exit_code = $null; binary_error = $null }
    try {
        $smoke.binary_output = "$(& $binary)".Trim()
        $smoke.binary_exit_code = $LASTEXITCODE
    } catch {
        $smoke.binary_error = $_.Exception.Message
    }
    $smoke | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $out 'report-smoke.json') -Encoding UTF8
    if ($bad.Count -gt 0) { Write-Host "Повреждённые файлы пакета: $($bad -join ', ')" -ForegroundColor Red }
    Write-Host "Проверка программы: $($smoke.binary_output) $($smoke.binary_error)"
    & (Join-Path $here 'scripts\server-probe.ps1') -OutDir $out
}

function Invoke-Collect {
    if ($Step -eq 'menu') {
        $note = Read-Host 'Что вы заметили? Можно оставить пустым'
        if ($note) { Add-Content -LiteralPath (Join-Path $out 'manual.txt') -Value $note -Encoding UTF8 }
    }
    Copy-Item -LiteralPath (Join-Path $here 'build.json') -Destination $out -Force
    & (Join-Path $here 'scripts\collect-logs.ps1') -OutDir $out -Destination (Split-Path -Parent $here) -Phase '00' -Build $build.build | Out-Null
}

function Invoke-Step([string]$Name) {
    switch ($Name) {
        'probe' { Invoke-Probe }
        'install' {
            if (-not (Test-IsAdmin)) { Write-Host 'Запустите PowerShell от имени администратора.' -ForegroundColor Yellow; return }
            & (Join-Path $here 'deploy\install-caddy.ps1') -PackageDir $here -OutDir $out
        }
        'udp' {
            $admin = Test-IsAdmin
            if (-not $admin) { Write-Host 'Без прав администратора брандмауэр не откроется, пакет может не дойти.' -ForegroundColor Yellow }
            $arguments = @{ OutDir = $out; Bind = $Bind; Seconds = $Seconds }
            if ($admin) { $arguments.OpenFirewall = $true }
            & (Join-Path $here 'scripts\udp-listen.ps1') @arguments
        }
        'collect' { Invoke-Collect }
        'rollback' { & (Join-Path $here 'deploy\uninstall-caddy.ps1') }
    }
}

if ($Step -ne 'menu') {
    Invoke-Step $Step
    exit 0
}

while ($true) {
    Write-Host ''
    Write-Host "RemoteConnectionSystem, фаза 0, сборка $($build.build)"
    Write-Host '1. Проверить пакет и собрать сведения о сервере'
    Write-Host '2. Установить Caddy и получить сертификат (администратор)'
    Write-Host '3. Проверить проброс 8443/UDP (администратор)'
    Write-Host '4. Собрать архив логов'
    Write-Host '5. Откатить установку Caddy (администратор)'
    Write-Host '0. Выход'
    $choice = Read-Host 'Выберите пункт'
    try {
        switch ($choice) {
            '1' { Invoke-Step 'probe' }
            '2' { Invoke-Step 'install' }
            '3' { Invoke-Step 'udp' }
            '4' { Invoke-Step 'collect' }
            '5' { Invoke-Step 'rollback' }
            '0' { exit 0 }
            default { Write-Host 'Нет такого пункта.' }
        }
    } catch {
        Write-Host "Ошибка: $($_.Exception.Message)" -ForegroundColor Red
    }
}
