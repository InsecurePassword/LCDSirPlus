# LCDForge 0.3.0 release packaging script (PowerShell 7+)
#Requires -Version 7

[CmdletBinding()]
param(
    [string]$OutputDir = '.\artifacts\release'
)

$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath (Split-Path $PSScriptRoot -Parent)
$version = '0.3.0'
$out = Join-Path $OutputDir "LCDForge-$version-win-x64"
$zip = Join-Path $OutputDir "LCDForge-$version-win-x64.zip"

Write-Host '== build ==' -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'build failed' }

if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Path $out | Out-Null

Write-Host '== assemble portable tree ==' -ForegroundColor Cyan
Copy-Item target\release\lcdforge.exe $out\
Copy-Item lcdforge.txt $out\
Copy-Item README.md $out\
Copy-Item docs $out\docs -Recurse

Write-Host '== clean-extraction validation ==' -ForegroundColor Cyan
& $out\lcdforge.exe --version
if ($LASTEXITCODE -ne 0) { throw 'portable exe failed' }
& $out\lcdforge.exe --validate-config
if ($LASTEXITCODE -ne 0) { throw 'portable config invalid' }

Write-Host '== package ==' -ForegroundColor Cyan
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $out\* -DestinationPath $zip

Write-Host '== SHA-256 ==' -ForegroundColor Cyan
$hashes = Get-ChildItem $out -Recurse -File | Get-FileHash -Algorithm SHA256
$manifest = Join-Path $OutputDir "LCDForge-$version-SHA256SUMS.txt"
$hashes | ForEach-Object { "{0}  {1}" -f $_.Hash.ToLower(), $_.Path.Substring($out.Length + 1) } | Set-Content $manifest
Get-Content $manifest
Write-Host "package: $zip" -ForegroundColor Green
