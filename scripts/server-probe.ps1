<#
.SYNOPSIS
  Writes system.json with the facts the project needs. Reads only.
.DESCRIPTION
  The external IP is requested from api.ipify.org only to compare it with DNS. The address is not written to the report.
#>
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$Domain = 'maxwimber.ru'
)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

function Invoke-Safe([scriptblock]$Action) {
    try { & $Action } catch { @{ error = $_.Exception.Message } }
}

function Get-DnsValues([string]$Type) {
    @(Resolve-DnsName -Name $Domain -Type $Type -DnsOnly -ErrorAction Stop |
        Where-Object { $_.Type -eq $Type } |
        ForEach-Object {
            $record = $_
            switch ($Type) {
                'A' { $record.IPAddress }
                'AAAA' { $record.IPAddress }
                'MX' { $record.NameExchange }
                'NS' { $record.NameHost }
                'TXT' { $record.Strings -join '' }
            }
        })
}

function Get-Listener([int[]]$TcpPorts, [int[]]$UdpPorts) {
    $tcp = @(Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue | Where-Object { $TcpPorts -contains $_.LocalPort })
    $udp = @(Get-NetUDPEndpoint -ErrorAction SilentlyContinue | Where-Object { $UdpPorts -contains $_.LocalPort })
    foreach ($item in $tcp + $udp) {
        $process = Get-Process -Id $item.OwningProcess -ErrorAction SilentlyContinue
        [ordered]@{
            protocol = if ($item -in $tcp) { 'tcp' } else { 'udp' }
            address  = $item.LocalAddress
            port     = $item.LocalPort
            process  = if ($process) { $process.ProcessName } else { $null }
        }
    }
}

$identity = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())

$report = [ordered]@{
    collected_at = (Get-Date).ToUniversalTime().ToString('o')
    is_admin     = $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    os           = Invoke-Safe {
        $os = Get-CimInstance Win32_OperatingSystem
        [ordered]@{ caption = $os.Caption; version = $os.Version; build = $os.BuildNumber; last_boot = $os.LastBootUpTime.ToUniversalTime().ToString('o') }
    }
    cpu          = Invoke-Safe { (Get-CimInstance Win32_Processor).Name }
    board        = Invoke-Safe {
        $board = Get-CimInstance Win32_BaseBoard
        [ordered]@{ manufacturer = $board.Manufacturer; product = $board.Product; bios = (Get-CimInstance Win32_BIOS).SMBIOSBIOSVersion }
    }
    gpus         = Invoke-Safe {
        @(Get-CimInstance Win32_VideoController | ForEach-Object {
                [ordered]@{
                    name    = $_.Name
                    driver  = $_.DriverVersion
                    date    = if ($_.DriverDate) { $_.DriverDate.ToString('yyyy-MM-dd') } else { $null }
                    width   = $_.CurrentHorizontalResolution
                    height  = $_.CurrentVerticalResolution
                    refresh = $_.CurrentRefreshRate
                }
            })
    }
    displays     = Invoke-Safe { @(Get-CimInstance -Namespace root\wmi -ClassName WmiMonitorBasicDisplayParams -ErrorAction Stop).Count }
    amf_runtime  = Invoke-Safe {
        $path = Join-Path $env:WINDIR 'System32\amfrt64.dll'
        if (Test-Path $path) { [ordered]@{ present = $true; version = (Get-Item $path).VersionInfo.FileVersion } } else { [ordered]@{ present = $false } }
    }
    memory       = Invoke-Safe {
        @(Get-CimInstance Win32_PhysicalMemory | ForEach-Object {
                [ordered]@{ part = "$($_.PartNumber)".Trim(); gib = [math]::Round($_.Capacity / 1GB, 1); speed = $_.Speed; slot = $_.DeviceLocator }
            })
    }
    disks        = Invoke-Safe { @(Get-PhysicalDisk | ForEach-Object { [ordered]@{ name = $_.FriendlyName; media = "$($_.MediaType)"; bus = "$($_.BusType)"; gb = [math]::Round($_.Size / 1GB); health = "$($_.HealthStatus)"; can_pool = $_.CanPool; usage = "$($_.Usage)" } }) }
    partitions   = Invoke-Safe { @(Get-Partition | ForEach-Object { [ordered]@{ disk = $_.DiskNumber; type = "$($_.Type)"; letter = "$($_.DriveLetter)"; gb = [math]::Round($_.Size / 1GB) } }) }
    diskpart_volumes = Invoke-Safe {
        if (-not $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { return 'requires administrator' }
        $script = [System.IO.Path]::GetTempFileName()
        try {
            Set-Content -LiteralPath $script -Value 'list volume' -Encoding ASCII
            @(& diskpart.exe /s $script | Where-Object { $_.Trim() })
        } finally {
            Remove-Item -LiteralPath $script -ErrorAction SilentlyContinue
        }
    }
    pools        = Invoke-Safe { @(Get-StoragePool -IsPrimordial $false -ErrorAction Stop | ForEach-Object { [ordered]@{ name = $_.FriendlyName; gb = [math]::Round($_.Size / 1GB); allocated_gb = [math]::Round($_.AllocatedSize / 1GB); health = "$($_.HealthStatus)" } }) }
    virtual_disks = Invoke-Safe { @(Get-VirtualDisk -ErrorAction Stop | ForEach-Object { [ordered]@{ name = $_.FriendlyName; resiliency = $_.ResiliencySettingName; redundancy = $_.PhysicalDiskRedundancy; gb = [math]::Round($_.Size / 1GB); health = "$($_.HealthStatus)" } }) }
    volumes      = Invoke-Safe { @(Get-Volume | Where-Object DriveLetter | ForEach-Object { [ordered]@{ letter = "$($_.DriveLetter)"; label = $_.FileSystemLabel; fs = $_.FileSystem; gb = [math]::Round($_.Size / 1GB); free_gb = [math]::Round($_.SizeRemaining / 1GB) } }) }
    network      = Invoke-Safe {
        @(Get-NetAdapter -Physical | Where-Object Status -eq 'Up' | ForEach-Object {
                $config = Get-NetIPConfiguration -InterfaceIndex $_.ifIndex
                [ordered]@{
                    name    = $_.InterfaceDescription
                    speed   = $_.LinkSpeed
                    ipv4    = $config.IPv4Address.IPAddress
                    gateway = $config.IPv4DefaultGateway.NextHop
                    profile = "$((Get-NetConnectionProfile -InterfaceIndex $_.ifIndex -ErrorAction SilentlyContinue).NetworkCategory)"
                }
            })
    }
    listeners    = Invoke-Safe { @(Get-Listener -TcpPorts 80, 443, 8080, 8444 -UdpPorts 443, 8443, 8444) }
    power        = Invoke-Safe {
        [ordered]@{ scheme = "$(powercfg /getactivescheme)"; standby = @(powercfg /query SCHEME_CURRENT SUB_SLEEP STANDBYIDLE | Where-Object { $_ -match '0x[0-9a-f]{8}\s*$' } | Select-Object -Last 2 | ForEach-Object { $_.Trim() }) }
    }
    vigem        = Invoke-Safe { $null -ne (Get-Service -Name ViGEmBus -ErrorAction SilentlyContinue) }
    dns          = Invoke-Safe {
        [ordered]@{
            domain = $Domain
            a      = Invoke-Safe { Get-DnsValues 'A' }
            aaaa   = Invoke-Safe { Get-DnsValues 'AAAA' }
            mx     = Invoke-Safe { Get-DnsValues 'MX' }
            ns     = Invoke-Safe { Get-DnsValues 'NS' }
            txt    = Invoke-Safe { Get-DnsValues 'TXT' }
        }
    }
    dns_matches_public_ip = Invoke-Safe {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $publicIp = "$(Invoke-RestMethod -Uri 'https://api.ipify.org' -TimeoutSec 10)".Trim()
        @(Get-DnsValues 'A') -contains $publicIp
    }
}

$path = Join-Path $OutDir 'system.json'
$report | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $path -Encoding UTF8
Write-Host "Сведения о системе записаны: $path"
