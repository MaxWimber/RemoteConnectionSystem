<#
.SYNOPSIS
  Phase 1 probe menu for the server. -Step runs one step without the menu.
#>
param(
    [ValidateSet('menu', 'probe', 'collect')][string]$Step = 'menu'
)

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$out = Join-Path $here 'out'
New-Item -ItemType Directory -Force -Path $out | Out-Null
$build = (Get-Content -LiteralPath (Join-Path $here 'build.json') -Raw -Encoding UTF8 | ConvertFrom-Json)

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
    $check = [ordered]@{ package_ok = $bad.Count -eq 0; damaged_files = $bad }
    $check | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $out 'report-package.json') -Encoding UTF8
    if ($bad.Count -gt 0) {
        Write-Host "Повреждённые файлы пакета: $($bad -join ', ')" -ForegroundColor Red
        return
    }
    Write-Host 'Сведения о сервере...'
    & (Join-Path $here 'scripts\server-probe.ps1') -OutDir $out
    Write-Host 'Проба захвата и кодировщиков. Не трогайте мышь около двух минут.'
    & (Join-Path $here 'bin\rcs-bench.exe') probe --out $out
    $code = $LASTEXITCODE
    [ordered]@{ exit_code = $code } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $out 'report-bench-exit.json') -Encoding UTF8
    if ($code -ne 0) { Write-Host "rcs-bench завершился с кодом $code" -ForegroundColor Yellow }
}

function Invoke-Collect {
    if ($Step -eq 'menu') {
        $note = Read-Host 'Что вы заметили? Можно оставить пустым'
        if ($note) { Add-Content -LiteralPath (Join-Path $out 'manual.txt') -Value $note -Encoding UTF8 }
    }
    Copy-Item -LiteralPath (Join-Path $here 'build.json') -Destination $out -Force
    & (Join-Path $here 'scripts\collect-logs.ps1') -OutDir $out -Destination (Split-Path -Parent $here) -Phase '01' -Build $build.build | Out-Null
}

function Invoke-Step([string]$Name) {
    switch ($Name) {
        'probe' { Invoke-Probe }
        'collect' { Invoke-Collect }
    }
}

if ($Step -ne 'menu') {
    Invoke-Step $Step
    exit 0
}

while ($true) {
    Write-Host ''
    Write-Host "RemoteConnectionSystem, фаза 1, сборка $($build.build)"
    Write-Host '1. Проверить пакет и запустить пробу'
    Write-Host '2. Собрать архив логов'
    Write-Host '0. Выход'
    $choice = Read-Host 'Выберите пункт'
    try {
        switch ($choice) {
            '1' { Invoke-Step 'probe' }
            '2' { Invoke-Step 'collect' }
            '0' { exit 0 }
            default { Write-Host 'Нет такого пункта.' }
        }
    } catch {
        Write-Host "Ошибка: $($_.Exception.Message)" -ForegroundColor Red
    }
}
