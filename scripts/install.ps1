<#
.SYNOPSIS
    Install memoria binary on Windows from GitHub releases.
.DESCRIPTION
    Downloads and installs memoria to a local directory and adds it to the user PATH.
.PARAMETER Version
    Version tag to install (default: latest release).
.PARAMETER Dir
    Install directory (default: $env:LOCALAPPDATA\Programs\memoria).
.PARAMETER Tool
    Auto-init after install (kiro|cursor|claude|codex|gemini).
.PARAMETER ApiUrl
    Memoria API URL for auto-init.
.PARAMETER Token
    Memoria API token for auto-init.
.PARAMETER Yes
    Skip confirmation prompt.
.EXAMPLE
    # Basic install
    irm https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.ps1 | iex

    # Install + init (one-liner via env vars)
    $env:MEMORIA_TOOL='cursor'; $env:MEMORIA_API_URL='http://api.example.com'; $env:MEMORIA_TOKEN='sk-xxx'; irm https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.ps1 | iex

    # Install + init (download then run with params)
    .\install.ps1 -Tool cursor -ApiUrl http://api.example.com -Token sk-xxx
#>
param(
    [string]$Version,
    [string]$Dir,
    [string]$Tool,
    [string]$ApiUrl,
    [string]$Token,
    [switch]$Yes
)

$ErrorActionPreference = 'Stop'

# ── Merge env vars into params (for piped iex usage) ────────────────

if (-not $Version -and $env:MEMORIA_VERSION)   { $Version = $env:MEMORIA_VERSION }
if (-not $Dir     -and $env:MEMORIA_DIR)       { $Dir     = $env:MEMORIA_DIR }
if (-not $Tool    -and $env:MEMORIA_TOOL)      { $Tool    = $env:MEMORIA_TOOL }
if (-not $ApiUrl  -and $env:MEMORIA_API_URL)   { $ApiUrl  = $env:MEMORIA_API_URL }
if (-not $Token   -and $env:MEMORIA_TOKEN)     { $Token   = $env:MEMORIA_TOKEN }

# ── Banner ──────────────────────────────────────────────────────────

Write-Host @"

 ███╗   ███╗███████╗███╗   ███╗ ██████╗ ██████╗ ██╗ █████╗
 ████╗ ████║██╔════╝████╗ ████║██╔═══██╗██╔══██╗██║██╔══██╗
 ██╔████╔██║█████╗  ██╔████╔██║██║   ██║██████╔╝██║███████║
 ██║╚██╔╝██║██╔══╝  ██║╚██╔╝██║██║   ██║██╔══██╗██║██╔══██║
 ██║ ╚═╝ ██║███████╗██║ ╚═╝ ██║╚██████╔╝██║  ██║██║██║  ██║
 ╚═╝     ╚═╝╚══════╝╚═╝     ╚═╝ ╚═════╝ ╚═╝  ╚═╝╚═╝╚═╝  ╚═╝
             Memoria - Secure · Auditable · Programmable Memory
"@

# ── Config ──────────────────────────────────────────────────────────

$Repo = if ($env:MEMORIA_REPO) { $env:MEMORIA_REPO } else { 'matrixorigin/Memoria' }
$Target = 'x86_64-pc-windows-msvc'
$Asset = "memoria-$Target.zip"

if (-not $Dir) {
    $Dir = Join-Path $env:LOCALAPPDATA 'Programs\memoria'
}

# ── Resolve version ─────────────────────────────────────────────────

if (-not $Version) { $Version = 'latest' }
if ($Version -eq 'latest') {
    $release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ 'User-Agent' = 'memoria-installer' }
    $Version = $release.tag_name
}

$DownloadUrl = "https://github.com/$Repo/releases/download/$Version/$Asset"
$ChecksumUrl = "https://github.com/$Repo/releases/download/$Version/SHA256SUMS.txt"

# ── Check existing installation ─────────────────────────────────────

$SkipDownload = $false
$existing = Get-Command memoria -ErrorAction SilentlyContinue
if ($existing) {
    $installed = (memoria --version 2>$null) -replace '.*?(\d+\.\d+\.\d+).*', '$1'
    $target_ver = $Version -replace '^v', ''
    if ($installed -eq $target_ver) {
        Write-Host "✓ memoria v$installed already installed (latest)" -ForegroundColor Green
        $Dir = Split-Path $existing.Source
        $SkipDownload = $true
    } else {
        Write-Host "> memoria v$installed installed, upgrading to v$target_ver"
    }
}

# ── Confirm ─────────────────────────────────────────────────────────

if (-not $SkipDownload) {
    Write-Host ""
    Write-Host "> Version:   $Version" -ForegroundColor Cyan
    Write-Host "> Platform:  $Target" -ForegroundColor Cyan
    Write-Host "> Directory: $Dir" -ForegroundColor Cyan
    Write-Host ""

    if (-not $Yes -and -not ($Tool -and $ApiUrl -and $Token)) {
        $yn = Read-Host 'Install memoria? [y/N]'
        if ($yn -notmatch '^[Yy]') {
            Write-Host '> Aborted'; exit 0
        }
    }

    # ── Download ────────────────────────────────────────────────────

    $tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP "memoria-install-$(Get-Random)")
    try {
        $zipPath = Join-Path $tmp $Asset
        Write-Host "> Downloading $DownloadUrl"
        Invoke-WebRequest -Uri $DownloadUrl -OutFile $zipPath -UseBasicParsing

        # ── Verify checksum ─────────────────────────────────────────

        try {
            $sumPath = Join-Path $tmp 'SHA256SUMS.txt'
            Invoke-WebRequest -Uri $ChecksumUrl -OutFile $sumPath -UseBasicParsing
            $expected = (Get-Content $sumPath | Where-Object { $_ -match $Asset }) -replace '\s+.*', ''
            $actual = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
            if ($expected -and $actual -eq $expected) {
                Write-Host "✓ Checksum verified" -ForegroundColor Green
            } elseif ($expected) {
                Write-Host "✗ Checksum mismatch" -ForegroundColor Red; exit 1
            }
        } catch {
            Write-Host "! Checksum file unavailable, skipping verification" -ForegroundColor Yellow
        }

        # ── Install ─────────────────────────────────────────────────

        New-Item -ItemType Directory -Force -Path $Dir | Out-Null
        Expand-Archive -Path $zipPath -DestinationPath $tmp -Force
        Copy-Item (Join-Path $tmp 'memoria.exe') (Join-Path $Dir 'memoria.exe') -Force

        Write-Host ""
        Write-Host "✓ Installed memoria.exe to $Dir\memoria.exe" -ForegroundColor Green
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

# ── Add to user PATH ───────────────────────────────────────────────

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($userPath -notlike "*$Dir*") {
    [Environment]::SetEnvironmentVariable('Path', "$Dir;$userPath", 'User')
    $env:Path = "$Dir;$env:Path"
    Write-Host "> Added $Dir to user PATH (restart terminal to take effect)" -ForegroundColor Yellow
}

# ── Auto-init ──────────────────────────────────────────────────────

$bin = Join-Path $Dir 'memoria.exe'
if ($Tool -and $ApiUrl -and $Token) {
    Write-Host ""
    Write-Host "> Running: memoria init --tool $Tool --api-url $ApiUrl --token ***"
    & $bin init --tool $Tool --api-url $ApiUrl --token $Token --force
} elseif ($Tool) {
    Write-Host ""
    Write-Host "> Running: memoria init -i --tool $Tool"
    & $bin init -i --tool $Tool
} else {
    Write-Host ""
    Write-Host "> Next: run 'memoria init -i' in your project directory to start the setup wizard" -ForegroundColor Cyan
}
