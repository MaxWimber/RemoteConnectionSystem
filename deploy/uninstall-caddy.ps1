<#
.SYNOPSIS
  Rolls back install-caddy.ps1. Keeps certificates and logs in ProgramData\RCS.
#>
$ErrorActionPreference = 'Stop'
$serviceName = 'rcs-caddy'
$ruleName = 'RCS Caddy HTTP and HTTPS'

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Нужны права администратора: откройте PowerShell от имени администратора.'
}

if (Get-Service -Name $serviceName -ErrorAction SilentlyContinue) {
    Stop-Service -Name $serviceName -Force
    & sc.exe delete $serviceName | Out-Null
}
Remove-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue

Write-Host 'Служба и правило брандмауэра удалены. Сертификаты и логи остались в ProgramData\RCS.' -ForegroundColor Green
