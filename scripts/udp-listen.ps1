<#
.SYNOPSIS
  Listens on a UDP port for a limited time and reports whether the test packet arrived.
.DESCRIPTION
  With -OpenFirewall the script adds a temporary inbound rule and always removes it at the end.
#>
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [int]$Port = 8443,
    [int]$Seconds = 180,
    [string]$Bind = '0.0.0.0',
    [switch]$OpenFirewall
)

$ErrorActionPreference = 'Stop'
$ruleName = 'RCS temporary UDP check'
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$received = 0
$matched = 0
$error_message = $null
$client = $null

try {
    if ($OpenFirewall) {
        New-NetFirewallRule -DisplayName $ruleName -Direction Inbound -Action Allow -Protocol UDP -LocalPort $Port -Profile Any | Out-Null
    }

    $endpoint = New-Object System.Net.IPEndPoint([System.Net.IPAddress]::Parse($Bind), $Port)
    $client = New-Object System.Net.Sockets.UdpClient($endpoint)
    $client.Client.ReceiveTimeout = 1000

    Write-Host "Жду UDP-пакет на порту $Port, не более $Seconds с..."
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline -and $matched -eq 0) {
        try {
            $remote = New-Object System.Net.IPEndPoint([System.Net.IPAddress]::Any, 0)
            $bytes = $client.Receive([ref]$remote)
            $received++
            if ([System.Text.Encoding]::ASCII.GetString($bytes) -eq 'rcs-test') { $matched++ }
        } catch [System.Net.Sockets.SocketException] {
            if ($_.Exception.SocketErrorCode -ne [System.Net.Sockets.SocketError]::TimedOut) { throw }
        }
    }
} catch {
    $error_message = $_.Exception.Message
} finally {
    if ($client) { $client.Close() }
    if ($OpenFirewall) { Remove-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue }
}

$report = [ordered]@{
    port            = $Port
    packets         = $received
    test_packet     = $matched -gt 0
    firewall_opened = [bool]$OpenFirewall
    error           = $error_message
}
$report | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $OutDir 'report-udp.json') -Encoding UTF8

if ($matched -gt 0) {
    Write-Host 'Тестовый пакет получен. Проброс работает.' -ForegroundColor Green
} else {
    Write-Host "Тестовый пакет не получен. $error_message" -ForegroundColor Yellow
}
