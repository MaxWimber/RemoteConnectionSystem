<#
.SYNOPSIS
  Phase 3 (P3-10): lets WebRTC media from the internet reach rcs-agent on 8443/UDP. Run as administrator.
.DESCRIPTION
  One inbound rule, UDP 8443, bound to rcs-agent.exe, any profile and any remote address. Opened only after the
  SECURITY.md checklist; the agent answers only sessions that rcs-web opened after sign-in. Safe to run again.

    powershell -ExecutionPolicy Bypass -File deploy\firewall-agent.ps1            # add or update the rule
    powershell -ExecutionPolicy Bypass -File deploy\firewall-agent.ps1 -Remove    # remove it
#>
param(
    [string]$AgentExe = (Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\rcs-agent.exe'),
    [switch]$Remove,
    [string]$OutDir
)

$ErrorActionPreference = 'Stop'
$ruleName = 'RCS agent media UDP 8443'
$report = [ordered]@{ removed = $false; rule = $null; error = $null }

try {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Нужны права администратора: откройте PowerShell от имени администратора.'
    }
    Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue | Remove-NetFirewallRule
    if ($Remove) {
        $report.removed = $true
    } else {
        if (-not (Test-Path -LiteralPath $AgentExe)) { throw "Нет файла агента: $AgentExe" }
        New-NetFirewallRule -DisplayName $ruleName -Direction Inbound -Action Allow -Protocol UDP -LocalPort 8443 `
            -Program $AgentExe -Profile Any | Out-Null
        $r = Get-NetFirewallRule -DisplayName $ruleName
        $report.rule = [ordered]@{
            enabled = "$($r.Enabled)"; profile = "$($r.Profile)"
            ports = ($r | Get-NetFirewallPortFilter).LocalPort -join ','
            protocol = ($r | Get-NetFirewallPortFilter).Protocol
            program = ($r | Get-NetFirewallApplicationFilter).Program
        }
    }
} catch {
    $report.error = $_.Exception.Message
    Write-Host "Ошибка: $($report.error)" -ForegroundColor Red
} finally {
    if ($OutDir) {
        New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
        $report | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $OutDir 'report-firewall-agent.json') -Encoding UTF8
    }
    $report | ConvertTo-Json -Depth 4 | Write-Host
}
