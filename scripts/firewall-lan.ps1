# Phase 2: lets the home network reach rcs-web (8080/TCP) and rcs-agent (8443/UDP) for testing.
# Needs administrator rights. Each rule is inbound, Private profile only, home subnet only and bound to its
# program, so nothing else can use the port and nothing outside the subnet is let in. The router forwards
# nothing to 8080, and 8443/UDP stays closed to the internet until the Gate of phase 3 (docs/SECURITY.md).
#
#   powershell -ExecutionPolicy Bypass -File scripts\firewall-lan.ps1            # add or update the rules
#   powershell -ExecutionPolicy Bypass -File scripts\firewall-lan.ps1 -Remove    # remove them
param(
    [string]$Subnet = '192.168.1.0/24',
    [string]$BinDir = (Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release'),
    [switch]$Remove,
    [string]$LogFile
)

$ErrorActionPreference = 'Stop'
$group = 'RCS phase 2 LAN test'

function Write-Log([string]$Text) {
    Write-Output $Text
    if ($LogFile) { Add-Content -Path $LogFile -Value $Text -Encoding UTF8 }
}

try {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run this script as administrator.'
    }

    Get-NetFirewallRule -Group $group -ErrorAction SilentlyContinue | Remove-NetFirewallRule
    if ($Remove) {
        Write-Log "Removed the rules of group '$group'."
        return
    }

    $rules = @(
        @{ Name = 'RCS rcs-web 8080/TCP LAN'; Protocol = 'TCP'; Port = 8080; Program = Join-Path $BinDir 'rcs-web.exe' },
        @{ Name = 'RCS rcs-agent 8443/UDP LAN'; Protocol = 'UDP'; Port = 8443; Program = Join-Path $BinDir 'rcs-agent.exe' }
    )
    foreach ($r in $rules) {
        New-NetFirewallRule -DisplayName $r.Name -Group $group -Direction Inbound -Action Allow `
            -Profile Private -Protocol $r.Protocol -LocalPort $r.Port -RemoteAddress $Subnet `
            -Program $r.Program | Out-Null
        Write-Log ("Added '{0}': {1} {2}, from {3}, program {4}" -f $r.Name, $r.Protocol, $r.Port, $Subnet, $r.Program)
    }
} catch {
    Write-Log "FAILED: $($_.Exception.Message)"
    exit 1
}
