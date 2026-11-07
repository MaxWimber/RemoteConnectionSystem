<#
.SYNOPSIS
  Builds the test package for one phase into dist\.
.DESCRIPTION
  The content comes from deploy\phases\NN\manifest.txt. Lines have one of these forms:
    file: repo\relative\path => path\inside\package
    dir: repo\relative\folder => folder\inside\package      the whole folder
    bin: crate-name
    web: folder\inside\package                               npm ci, npm run build, then web\dist goes there
  Programs get RCS_BUILD_ID, the build number (date, time, commit) shown instead of "dev" (D-035).
#>
param(
    [Parameter(Mandatory = $true)][int]$Phase
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$phaseId = '{0:00}' -f $Phase
$phaseDir = Join-Path $root "deploy\phases\$phaseId"
$manifest = Join-Path $phaseDir 'manifest.txt'
if (-not (Test-Path $manifest)) { throw "Missing manifest: $manifest" }

function Get-BuildId {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmm'
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $commit = & git -C $root rev-parse --short HEAD 2>$null
        if ($LASTEXITCODE -ne 0 -or -not $commit) { return "$stamp-nocommit" }
        $dirty = & git -C $root status --porcelain
        if ($dirty) { return "$stamp-$commit-dirty" }
        return "$stamp-$commit"
    } finally {
        $ErrorActionPreference = $previous
    }
}

$build = Get-BuildId
$name = "rcs-phase$phaseId-$build"
$dist = Join-Path $root 'dist'
$stage = Join-Path $dist "stage\$name"
New-Item -ItemType Directory -Force -Path $stage | Out-Null

$entries = Get-Content -LiteralPath $manifest -Encoding UTF8 |
    ForEach-Object { $_.Trim() } |
    Where-Object { $_ -and -not $_.StartsWith('#') }

$binaries = @($entries | Where-Object { $_ -like 'bin:*' } | ForEach-Object { $_.Substring(4).Trim() })
if ($binaries.Count -gt 0) {
    $env:RCS_BUILD_ID = $build
    try {
        $packages = @($binaries | ForEach-Object { '-p', $_ })
        & cargo build --release @packages
        if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    } finally {
        Remove-Item Env:\RCS_BUILD_ID -ErrorAction SilentlyContinue
    }
    $binDir = Join-Path $stage 'bin'
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    foreach ($binary in $binaries) {
        Copy-Item -LiteralPath (Join-Path $root "target\release\$binary.exe") -Destination $binDir
        $symbols = Join-Path $root ("target\release\{0}.pdb" -f ($binary -replace '-', '_'))
        if (Test-Path $symbols) { Copy-Item -LiteralPath $symbols -Destination $binDir }
    }
}

foreach ($entry in ($entries | Where-Object { $_ -like 'web:*' })) {
    $web = Join-Path $root 'web'
    Push-Location $web
    try {
        & npm ci --no-audit --no-fund
        if ($LASTEXITCODE -ne 0) { throw 'npm ci failed' }
        & npm run build
        if ($LASTEXITCODE -ne 0) { throw 'npm run build failed' }
    } finally {
        Pop-Location
    }
    $target = Join-Path $stage $entry.Substring(4).Trim()
    New-Item -ItemType Directory -Force -Path $target | Out-Null
    Copy-Item -Path (Join-Path $web 'dist\*') -Destination $target -Recurse
}

foreach ($entry in ($entries | Where-Object { $_ -like 'dir:*' })) {
    $parts = $entry.Substring(4) -split '=>'
    if ($parts.Count -ne 2) { throw "Bad manifest line: $entry" }
    $target = Join-Path $stage $parts[1].Trim()
    New-Item -ItemType Directory -Force -Path $target | Out-Null
    Copy-Item -Path (Join-Path (Join-Path $root $parts[0].Trim()) '*') -Destination $target -Recurse
}

foreach ($entry in ($entries | Where-Object { $_ -like 'file:*' })) {
    $parts = $entry.Substring(5) -split '=>'
    if ($parts.Count -ne 2) { throw "Bad manifest line: $entry" }
    $source = Join-Path $root $parts[0].Trim()
    $target = Join-Path $stage $parts[1].Trim()
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
    Copy-Item -LiteralPath $source -Destination $target
}

Get-ChildItem -Path $stage -Recurse -File -Include *.html, *.txt | ForEach-Object {
    $text = [System.IO.File]::ReadAllText($_.FullName)
    if ($text.Contains('RCS_BUILD_ID')) {
        [System.IO.File]::WriteAllText($_.FullName, $text.Replace('RCS_BUILD_ID', $build), (New-Object System.Text.UTF8Encoding($true)))
    }
}

$info = [ordered]@{ phase = $Phase; build = $build; created_at = (Get-Date).ToUniversalTime().ToString('o') }
$info | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $stage 'build.json') -Encoding UTF8

$sums = Get-ChildItem -Path $stage -Recurse -File | Sort-Object FullName | ForEach-Object {
    $relative = $_.FullName.Substring($stage.Length + 1)
    '{0}  {1}' -f (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(), $relative
}
$sums | Set-Content -LiteralPath (Join-Path $stage 'SHA256SUMS.txt') -Encoding UTF8

$archive = Join-Path $dist "$name.zip"
if (Test-Path $archive) { Remove-Item -LiteralPath $archive }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $archive
Remove-Item -LiteralPath $stage -Recurse -Force

$hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "Package: $archive"
Write-Host "Size:    $([math]::Round((Get-Item $archive).Length / 1MB, 2)) MiB"
Write-Host "SHA256:  $hash"
