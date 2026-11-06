<#
.SYNOPSIS
  Phase 6 (P6-8, D-025): installs RCS as Windows services, or removes them with -Uninstall. Run as administrator.
.DESCRIPTION
  Install (safe to run again, it updates):
    1. Copies rcs-supervisor.exe, rcs-agent.exe, rcs-web.exe from target\release and web\dist to C:\Program Files\RCS.
    2. Creates or updates the services: rcs-web under NT SERVICE\rcs-web, rcs-supervisor under LocalSystem; both start
       automatically and restart after a failure.
    3. Gives NT SERVICE\rcs-web full control of the secrets, modify on the logs, read on the program folder.
    4. Registers the event source "RCS Supervisor", points the 8443/UDP firewall rule at the installed agent.
    5. Starts both services and checks: rcs-web listens on 127.0.0.1:8080, the supervisor started the agent, Caddy
       answers 401 without a session.
  Uninstall: stops and deletes both services, removes C:\Program Files\RCS and the folder rights of the service
  account, points the firewall rule back at target\release\rcs-agent.exe. Secrets and logs stay. Afterwards rcs-web and
  rcs-agent run by hand as in phases 3 to 5.

    powershell -ExecutionPolicy Bypass -File deploy\install.ps1
    powershell -ExecutionPolicy Bypass -File deploy\install.ps1 -Uninstall
#>
param(
    [switch]$Uninstall,
    [string]$InstallDir = 'C:\Program Files\RCS',
    [string]$Advertise = '95.31.119.180:8443,192.168.1.12:8443,127.0.0.1:8443',
    [ValidateSet('record', 'sendinput')][string]$InputMode = 'sendinput',
    [string]$OutDir
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path $repo 'out\phase-06\install' }
$data = Join-Path $env:ProgramData 'RCS'
$secrets = Join-Path $data 'secrets'
$logs = Join-Path $data 'logs'
$account = 'NT SERVICE\rcs-web'
$ruleName = 'RCS agent media UDP 8443'
$report = [ordered]@{ action = $(if ($Uninstall) { 'uninstall' } else { 'install' }); steps = @(); checks = [ordered]@{}; error = $null }

function Step([string]$text) {
    Write-Host "• $text"
    $report.steps += $text
}

# Runs a native program; a non-zero exit code is an error unless it is in -Allow.
function Native([string]$exe, [string[]]$arguments, [int[]]$Allow = @(0)) {
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { $out = & $exe @arguments 2>&1 | ForEach-Object { "$_" } } finally { $ErrorActionPreference = $old }
    if ($Allow -notcontains $LASTEXITCODE) { throw "$exe $($arguments -join ' '): код $LASTEXITCODE`n$($out -join "`n")" }
    $out
}

# sc.exe with its command line exactly as written: PowerShell 5.1 breaks arguments with inner quotes.
function Sc([string]$line) {
    $out = Join-Path $env:TEMP 'rcs-sc.txt'
    $p = Start-Process -FilePath sc.exe -ArgumentList $line -NoNewWindow -Wait -PassThru -RedirectStandardOutput $out
    $text = Get-Content -LiteralPath $out -Raw -ErrorAction SilentlyContinue
    if ($p.ExitCode -ne 0) { throw "sc.exe ${line}: код $($p.ExitCode)`n$text" }
}

function Remove-ServiceIfPresent([string]$name) {
    $s = Get-Service -Name $name -ErrorAction SilentlyContinue
    if (-not $s) { return }
    if ($s.Status -ne 'Stopped') {
        Stop-Service -Name $name -Force
        $s.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30))
    }
    Sc "delete $name"
    Step "служба $name удалена"
}

function Set-Service([string]$name, [string]$binPath, [string]$obj, [string]$display, [string]$description) {
    $exists = Get-Service -Name $name -ErrorAction SilentlyContinue
    if ($exists) {
        if ($exists.Status -ne 'Stopped') {
            Stop-Service -Name $name -Force
            $exists.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30))
        }
        Sc "config $name binPath= $binPath obj= `"$obj`" start= auto DisplayName= `"$display`""
        Step "служба $name обновлена"
    } else {
        Sc "create $name binPath= $binPath obj= `"$obj`" start= auto DisplayName= `"$display`""
        Step "служба $name создана"
    }
    Sc "description $name `"$description`""
    # Restart after 5 s, 10 s, then every 30 s; the failure count resets after a day.
    Sc "failure $name reset= 86400 actions= restart/5000/restart/10000/restart/30000"
    Sc "failureflag $name 1"
}

function Set-AgentFirewallProgram([string]$program) {
    $rule = Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
    if ($rule) {
        $rule | Get-NetFirewallApplicationFilter | Set-NetFirewallApplicationFilter -Program $program
    } else {
        New-NetFirewallRule -DisplayName $ruleName -Direction Inbound -Action Allow -Protocol UDP -LocalPort 8443 `
            -Program $program -Profile Any | Out-Null
    }
    Step "правило брандмауэра 8443/UDP: $program"
}

try {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Нужны права администратора: запустите PowerShell от имени администратора.'
    }

    if ($Uninstall) {
        Remove-ServiceIfPresent 'rcs-supervisor'
        Remove-ServiceIfPresent 'rcs-web'
        Get-Process rcs-agent -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$InstallDir*" } | Stop-Process -Force
        foreach ($dir in @($secrets, $logs)) {
            if (Test-Path $dir) { Native icacls.exe @($dir, '/remove:g', $account, '/T', '/C', '/Q') -Allow @(0, 1332) | Out-Null }
        }
        Step 'права учётной записи службы на секреты и журналы сняты'
        if (Test-Path $InstallDir) {
            Remove-Item -LiteralPath $InstallDir -Recurse -Force
            Step "$InstallDir удалён"
        }
        Set-AgentFirewallProgram (Join-Path $repo 'target\release\rcs-agent.exe')
        $report.checks.services_left = @(Get-Service rcs-web, rcs-supervisor -ErrorAction SilentlyContinue).Count
        $report.checks.secrets_kept = Test-Path (Join-Path $secrets 'auth.json')
        Write-Host 'Удалено. Ручной запуск: target\release\rcs-web.exe и rcs-agent.exe, как в фазах 3–5.' -ForegroundColor Green
        return
    }

    $release = Join-Path $repo 'target\release'
    $dist = Join-Path $repo 'web\dist'
    foreach ($f in @('rcs-supervisor.exe', 'rcs-agent.exe', 'rcs-web.exe')) {
        if (-not (Test-Path (Join-Path $release $f))) { throw "Нет $f в ${release}: соберите cargo build --release --workspace." }
    }
    if (-not (Test-Path (Join-Path $dist 'index.html'))) { throw "Нет собранного клиента в ${dist}: npm run build." }
    if (-not (Test-Path (Join-Path $secrets 'auth.json'))) { throw 'Секреты не настроены: rcs-web set-password и enroll-totp.' }
    $manual = Get-Process rcs-agent, rcs-web -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$release*" }
    if ($manual) { throw "Остановите вручную запущенные $(($manual | ForEach-Object Name) -join ', '): они заняли порты и канал." }

    # Services stop first, so their files can be replaced.
    foreach ($name in 'rcs-supervisor', 'rcs-web') {
        $s = Get-Service -Name $name -ErrorAction SilentlyContinue
        if ($s -and $s.Status -ne 'Stopped') { Stop-Service -Name $name -Force; $s.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30)) }
    }
    New-Item -ItemType Directory -Force -Path $InstallDir, (Join-Path $InstallDir 'web'), $logs | Out-Null
    foreach ($f in @('rcs-supervisor.exe', 'rcs-agent.exe', 'rcs-web.exe')) {
        Copy-Item -LiteralPath (Join-Path $release $f) -Destination $InstallDir -Force
    }
    Remove-Item -Path (Join-Path $InstallDir 'web\*') -Recurse -Force -ErrorAction SilentlyContinue
    Copy-Item -Path (Join-Path $dist '*') -Destination (Join-Path $InstallDir 'web') -Recurse -Force
    Step "файлы скопированы в $InstallDir"

    $web = Join-Path $InstallDir 'rcs-web.exe'
    $sup = Join-Path $InstallDir 'rcs-supervisor.exe'
    $webBin = "`"\`"$web\`" service --static \`"$(Join-Path $InstallDir 'web')\`"`""
    $agentLog = Join-Path $logs 'agent'
    $supBin = "`"\`"$sup\`" service -- --udp 0.0.0.0:8443 --advertise $Advertise --input $InputMode --log \`"$agentLog\`"`""
    Set-Service 'rcs-web' $webBin $account 'RCS web' 'RemoteConnectionSystem: вход, сигналинг, веб-клиент на 127.0.0.1:8080.'
    Set-Service 'rcs-supervisor' $supBin 'LocalSystem' 'RCS supervisor' 'RemoteConnectionSystem: запускает агента захвата и ввода в консольной сессии.'
    # The supervisor follows console sessions.
    Sc 'sidtype rcs-web unrestricted'

    Native icacls.exe @($secrets, '/grant', "${account}:(OI)(CI)F", '/T', '/C', '/Q') | Out-Null
    Native icacls.exe @($logs, '/grant', "${account}:(OI)(CI)M", '/T', '/C', '/Q') | Out-Null
    Native icacls.exe @($InstallDir, '/grant', "${account}:(OI)(CI)RX", '/T', '/C', '/Q') | Out-Null
    Step "права ${account}: секреты F, журналы M, программа RX"

    if (-not [Diagnostics.EventLog]::SourceExists('RCS Supervisor')) {
        New-EventLog -LogName Application -Source 'RCS Supervisor'
        Step 'источник журнала событий RCS Supervisor создан'
    }
    Set-AgentFirewallProgram (Join-Path $InstallDir 'rcs-agent.exe')

    Start-Service -Name 'rcs-web'
    Start-Service -Name 'rcs-supervisor'
    Step 'службы запущены'

    $listening = $false
    for ($i = 0; $i -lt 60 -and -not $listening; $i++) {
        $listening = [bool](Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort 8080 -State Listen -ErrorAction SilentlyContinue)
        if (-not $listening) { Start-Sleep -Seconds 1 }
    }
    $report.checks.web_listening = $listening
    $agent = $null
    for ($i = 0; $i -lt 30 -and -not $agent; $i++) {
        $agent = Get-CimInstance Win32_Process -Filter "Name='rcs-agent.exe'" | Where-Object { $_.ExecutablePath -like "$InstallDir*" }
        if (-not $agent) { Start-Sleep -Seconds 1 }
    }
    $report.checks.agent_running = [bool]$agent
    if ($agent) {
        $owner = Invoke-CimMethod -InputObject ($agent | Select-Object -First 1) -MethodName GetOwner
        $report.checks.agent_user = "$($owner.Domain)\$($owner.User)"
        $report.checks.agent_session = ($agent | Select-Object -First 1).SessionId
    }
    $webProc = Get-CimInstance Win32_Process -Filter "Name='rcs-web.exe'" | Where-Object { $_.ExecutablePath -like "$InstallDir*" } | Select-Object -First 1
    if ($webProc) {
        $owner = Invoke-CimMethod -InputObject $webProc -MethodName GetOwner
        $report.checks.web_user = "$($owner.Domain)\$($owner.User)"
    }
    $report.checks.services = @(Get-Service rcs-web, rcs-supervisor | ForEach-Object { "$($_.Name)=$($_.Status)" })
    $code = Native curl.exe @('-s', '-o', 'NUL', '-w', '%{http_code}', '--resolve', 'maxwimber.ru:8444:127.0.0.1',
        'https://maxwimber.ru:8444/RemoteConnectionSystem/api/status') -Allow @(0, 7, 28, 35, 56)
    $report.checks.caddy_status_without_session = "$code"
    if (-not $listening -or -not $agent -or "$code" -ne '401') {
        throw "Проверка после установки не прошла: $($report.checks | ConvertTo-Json -Compress)"
    }
    Write-Host 'Установлено: rcs-web и rcs-supervisor работают, агент запущен.' -ForegroundColor Green
} catch {
    $report.error = $_.Exception.Message
    Write-Host "Ошибка: $($report.error)" -ForegroundColor Red
} finally {
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $name = if ($Uninstall) { 'report-uninstall.json' } else { 'report-install.json' }
    $report | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $OutDir $name) -Encoding UTF8
    $report | ConvertTo-Json -Depth 4 | Write-Host
}
