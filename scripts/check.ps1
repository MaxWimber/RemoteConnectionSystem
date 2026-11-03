<#
.SYNOPSIS
  Runs every local check. Exit code 0 means all selected checks passed.
#>
param(
    [switch]$SkipWeb,
    [switch]$SkipRust
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$failed = New-Object System.Collections.Generic.List[string]

function Invoke-Check([string]$Name, [scriptblock]$Action) {
    Write-Host "== $Name"
    try {
        $global:LASTEXITCODE = 0
        & $Action
        if ($LASTEXITCODE -ne 0) { throw "exit code $LASTEXITCODE" }
        Write-Host "OK $Name"
    } catch {
        Write-Host "FAILED $Name : $($_.Exception.Message)" -ForegroundColor Red
        $failed.Add($Name)
    }
}

Push-Location $root
try {
    Invoke-Check 'PowerShell scripts' {
        $scripts = Get-ChildItem -Path $root -Recurse -File -Filter *.ps1 |
            Where-Object { $_.FullName -notmatch '\\(target|node_modules|dist)\\' }
        foreach ($script in $scripts) {
            $tokens = $null
            $errors = $null
            [void][System.Management.Automation.Language.Parser]::ParseFile($script.FullName, [ref]$tokens, [ref]$errors)
            if ($errors.Count -gt 0) { throw "$($script.Name): $($errors[0].Message)" }

            # Windows PowerShell 5.1 reads UTF-8 without a BOM as the ANSI code page.
            $bytes = [System.IO.File]::ReadAllBytes($script.FullName)
            $hasBom = $bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF
            $isAscii = -not ($bytes | Where-Object { $_ -gt 127 } | Select-Object -First 1)
            if (-not $hasBom -and -not $isAscii) { throw "$($script.Name): non-ASCII file needs a UTF-8 BOM" }
        }
    }

    if (-not $SkipRust) {
        Invoke-Check 'cargo fmt' { cargo fmt --all --check }
        Invoke-Check 'cargo clippy' { cargo clippy --workspace --all-targets -- -D warnings }
        Invoke-Check 'cargo test' { cargo test --workspace }
    }

    if (-not $SkipWeb) {
        if (-not (Get-Command npm -ErrorAction SilentlyContinue)) {
            Write-Host 'FAILED web : Node.js is not installed' -ForegroundColor Red
            $failed.Add('web')
        } else {
            Push-Location (Join-Path $root 'web')
            try {
                $install = if (Test-Path 'package-lock.json') { 'ci' } else { 'install' }
                Invoke-Check "npm $install" { npm $install }
                Invoke-Check 'typecheck' { npm run typecheck }
                Invoke-Check 'web tests' { npm test }
                Invoke-Check 'web build' { npm run build }
            } finally {
                Pop-Location
            }
        }
    }
} finally {
    Pop-Location
}

if ($failed.Count -gt 0) {
    Write-Host "Failed checks: $($failed -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host 'All checks passed.'
exit 0
