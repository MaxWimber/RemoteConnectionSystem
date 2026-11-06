<#
.SYNOPSIS
  Phase 6 Gate checks that need administrator rights, and the Ctrl+Alt+Del policy (P6-7). Run as administrator.
.DESCRIPTION
  Without switches:
    1. The accounts: rcs-web runs as NT SERVICE\rcs-web and is not an administrator; the agent runs as SYSTEM.
    2. Ends the agent by force and measures how long the supervisor takes to start it again.
  -EnableSas: sets SoftwareSASGeneration = 1 (services may send Ctrl+Alt+Del), keeping the old value for -RestoreSas.
  -RestoreSas: puts the old value back, or removes the value if there was none.
  Writes out\phase-06\check\report-check.json (or report-sas.json).

    powershell -ExecutionPolicy Bypass -File deploy\phase6-check.ps1
    powershell -ExecutionPolicy Bypass -File deploy\phase6-check.ps1 -EnableSas
    powershell -ExecutionPolicy Bypass -File deploy\phase6-check.ps1 -RestoreSas
#>
param(
    [switch]$EnableSas,
    [switch]$RestoreSas,
    [string]$InstallDir = 'C:\Program Files\RCS',
    [string]$OutDir
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path $repo 'out\phase-06\check' }
$policy = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
$backup = Join-Path $env:ProgramData 'RCS\sas-policy-before.json'
$report = [ordered]@{ checks = [ordered]@{}; error = $null }

function Owner($proc) {
    $o = Invoke-CimMethod -InputObject $proc -MethodName GetOwner
    "$($o.Domain)\$($o.User)"
}

function Agent() {
    Get-CimInstance Win32_Process -Filter "Name='rcs-agent.exe'" | Where-Object { $_.ExecutablePath -like "$InstallDir*" } | Select-Object -First 1
}

try {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Нужны права администратора.'
    }

    if ($EnableSas -or $RestoreSas) {
        $current = (Get-ItemProperty -Path $policy -Name SoftwareSASGeneration -ErrorAction SilentlyContinue).SoftwareSASGeneration
        if ($EnableSas) {
            if (-not (Test-Path $backup)) {
                @{ value = $current } | ConvertTo-Json | Set-Content -LiteralPath $backup -Encoding UTF8
            }
            Set-ItemProperty -Path $policy -Name SoftwareSASGeneration -Value 1 -Type DWord
            $report.checks.sas_before = $current
            $report.checks.sas_now = 1
            Write-Host 'SoftwareSASGeneration = 1: службы могут отправлять Ctrl+Alt+Del.' -ForegroundColor Green
        } else {
            $before = if (Test-Path $backup) { (Get-Content -LiteralPath $backup -Raw | ConvertFrom-Json).value } else { $null }
            if ($null -eq $before) {
                Remove-ItemProperty -Path $policy -Name SoftwareSASGeneration -ErrorAction SilentlyContinue
            } else {
                Set-ItemProperty -Path $policy -Name SoftwareSASGeneration -Value ([int]$before) -Type DWord
            }
            Remove-Item -LiteralPath $backup -ErrorAction SilentlyContinue
            $report.checks.sas_restored_to = $before
            Write-Host 'Прежнее значение SoftwareSASGeneration возвращено.' -ForegroundColor Green
        }
        return
    }

    # 1. Accounts.
    $web = Get-CimInstance Win32_Process -Filter "Name='rcs-web.exe'" | Where-Object { $_.ExecutablePath -like "$InstallDir*" } | Select-Object -First 1
    if (-not $web) { throw 'rcs-web из службы не запущен.' }
    $report.checks.web_user = Owner $web
    $admins = @(Get-LocalGroupMember -SID 'S-1-5-32-544' | ForEach-Object { "$($_.Name)" })
    $report.checks.web_is_admin = $admins -contains $report.checks.web_user
    $report.checks.web_is_system = $report.checks.web_user -match 'SYSTEM|СИСТЕМА'
    $report.checks.web_service_account = (Get-CimInstance Win32_Service -Filter "Name='rcs-web'").StartName
    $report.checks.supervisor_account = (Get-CimInstance Win32_Service -Filter "Name='rcs-supervisor'").StartName

    # 2. Forced end of the agent.
    $agent = Agent
    if (-not $agent) { throw 'Агент не запущен.' }
    $report.checks.agent_user = Owner $agent
    $report.checks.agent_pid_before = $agent.ProcessId
    Stop-Process -Id $agent.ProcessId -Force
    $killed = Get-Date
    $next = $null
    while (-not $next -and ((Get-Date) - $killed).TotalSeconds -lt 60) {
        Start-Sleep -Milliseconds 250
        $a = Agent
        if ($a -and $a.ProcessId -ne $report.checks.agent_pid_before) { $next = $a }
    }
    $report.checks.agent_restarted = [bool]$next
    if ($next) {
        $report.checks.agent_pid_after = $next.ProcessId
        $report.checks.agent_restart_seconds = [math]::Round(((Get-Date) - $killed).TotalSeconds, 1)
        $report.checks.agent_user_after = Owner $next
    }
    $ok = $report.checks.agent_restarted -and -not $report.checks.web_is_admin -and -not $report.checks.web_is_system
    if (-not $ok) { throw "Проверка не прошла: $($report.checks | ConvertTo-Json -Compress)" }
    Write-Host "Агент перезапущен через $($report.checks.agent_restart_seconds) с, rcs-web: $($report.checks.web_user)." -ForegroundColor Green
} catch {
    $report.error = $_.Exception.Message
    Write-Host "Ошибка: $($report.error)" -ForegroundColor Red
} finally {
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $name = if ($EnableSas -or $RestoreSas) { 'report-sas.json' } else { 'report-check.json' }
    $report | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $OutDir $name) -Encoding UTF8
    $report | ConvertTo-Json -Depth 4 | Write-Host
}
