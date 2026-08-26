# LCDForge 0.3.0 build and test script (PowerShell 7+)
#Requires -Version 7

[CmdletBinding()]
param(
    [switch]$ReleaseOnly
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
Set-Location -LiteralPath $repo

Write-Host '== cargo fmt check ==' -ForegroundColor Cyan
cargo fmt --check
if ($LASTEXITCODE -ne 0) { throw 'cargo fmt check failed' }

Write-Host '== cargo clippy (zero-warning policy) ==' -ForegroundColor Cyan
cargo clippy --all-targets 2>&1 | Tee-Object -Variable clippyOutput
if ($LASTEXITCODE -ne 0) { throw 'cargo clippy failed' }
$warnings = @($clippyOutput | Where-Object { $_ -match 'warning:' })
if ($warnings.Count -gt 0) { throw "clippy emitted $($warnings.Count) warnings; zero-warning policy violated" }

Write-Host '== cargo test ==' -ForegroundColor Cyan
cargo test --release
if ($LASTEXITCODE -ne 0) { throw 'cargo test failed' }

if ($ReleaseOnly) { return }

Write-Host '== release build ==' -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'release build failed' }

Write-Host '== smoke: --version ==' -ForegroundColor Cyan
& (Join-Path $repo 'target\release\lcdforge.exe') --version
if ($LASTEXITCODE -ne 0) { throw '--version failed' }

Write-Host '== smoke: --validate-config ==' -ForegroundColor Cyan
& (Join-Path $repo 'target\release\lcdforge.exe') --validate-config --config (Join-Path $repo 'lcdforge.txt')
if ($LASTEXITCODE -ne 0) { throw 'shipped configuration is invalid' }

Write-Host '== smoke: --hardware-discover (read-only) ==' -ForegroundColor Cyan
& (Join-Path $repo 'target\release\lcdforge.exe') --hardware-discover
Write-Host '   (exit code 3 = no G13 present; acceptable on machines without the device)'

Write-Host '== smoke: --hardware-test --backend virtual ==' -ForegroundColor Cyan
& (Join-Path $repo 'target\release\lcdforge.exe') --hardware-test --backend virtual --duration-secs 1
if ($LASTEXITCODE -ne 0) { throw 'virtual hardware test failed' }

Write-Host ''
Write-Host 'ALL GATES PASSED' -ForegroundColor Green
Write-Host 'Physical acceptance (human) is intentionally NOT part of this script:'
Write-Host '  .\target\release\lcdforge.exe --hardware-test --backend hid --duration-secs 60'
