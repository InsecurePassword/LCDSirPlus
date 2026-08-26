#Requires -Version 7.0
[CmdletBinding()]
param([string]$OutputDir = '.\artifacts\release')

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
$repo = Split-Path $PSScriptRoot -Parent
Set-Location -LiteralPath $repo
. (Join-Path $PSScriptRoot 'Package.Common.ps1')

$version = '0.3.0'
$artifactsRoot = [IO.Path]::GetFullPath((Join-Path $repo 'artifacts'))
[IO.Directory]::CreateDirectory($artifactsRoot) | Out-Null
$output = [IO.Path]::GetFullPath((Join-Path $repo $OutputDir))
if (-not ($output + '\').StartsWith($artifactsRoot.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'release output must remain beneath artifacts/'
}
if ((git status --porcelain).Count -ne 0) { throw 'release build requires a clean worktree' }
$head = (git rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $head -notmatch '^[0-9a-f]{40}$') { throw 'cannot resolve release commit' }

function Copy-Allowlist {
    param([string]$SourceRoot, [string]$Destination, [string[]]$Paths)
    foreach ($relative in $Paths) {
        $source = Join-Path $SourceRoot $relative.Replace('/', '\')
        $target = Join-Path $Destination $relative.Replace('/', '\')
        [IO.Directory]::CreateDirectory((Split-Path $target -Parent)) | Out-Null
        Copy-Item -LiteralPath $source -Destination $target
    }
}

function Write-PackageManifest {
    param([string]$Root)
    $files = @(Get-ChildItem -LiteralPath $Root -File -Force -Recurse | Where-Object { $_.Name -ne 'PACKAGE-MANIFEST.txt' })
    $relative = @($files | ForEach-Object { Get-RelativePackagePath -Root $Root -Path $_.FullName })
    $relative = @(Get-OrdinalSorted $relative)
    $lines = @()
    foreach ($name in $relative) {
        Assert-SafeRelativePath $name
        $path = Join-Path $Root $name.Replace('/', '\')
        $size = (Assert-RegularSingleLinkFile $path).Size
        $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        $lines += "{0}`t{1}`t{2}" -f $hash, $size, $name
    }
    [IO.File]::WriteAllLines((Join-Path $Root 'PACKAGE-MANIFEST.txt'), $lines, (New-Object Text.UTF8Encoding($false)))
    [void]@(Read-VerifiedManifest -Root $Root)
}

function Write-DeterministicZip {
    param([string]$Root, [string]$Archive, [string]$Prefix)
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $files = @(Get-ChildItem -LiteralPath $Root -File -Force -Recurse)
    $relative = @($files | ForEach-Object { Get-RelativePackagePath -Root $Root -Path $_.FullName })
    $relative = @(Get-OrdinalSorted $relative)
    $stream = [IO.File]::Open($Archive, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    try {
        $zip = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
        try {
            foreach ($name in $relative) {
                $entry = $zip.CreateEntry(($Prefix + '/' + $name), [IO.Compression.CompressionLevel]::Optimal)
                $entry.LastWriteTime = [DateTimeOffset]::new(2000, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
                $input = [IO.File]::OpenRead((Join-Path $Root $name.Replace('/', '\')))
                $outputStream = $entry.Open()
                try { $input.CopyTo($outputStream) }
                finally { $outputStream.Dispose(); $input.Dispose() }
            }
        }
        finally { $zip.Dispose() }
    }
    finally { $stream.Dispose() }
}

Write-Host '== quality gate ==' -ForegroundColor Cyan
& (Join-Path $PSScriptRoot 'Test.ps1')
if ($LASTEXITCODE -ne 0) { throw 'quality gate failed' }
if ((git status --porcelain).Count -ne 0 -or (git rev-parse HEAD).Trim() -ne $head) { throw 'source changed during quality gate' }

if ([IO.Directory]::Exists($output)) { Remove-Item -LiteralPath $output -Recurse -Force }
[IO.Directory]::CreateDirectory($output) | Out-Null
$work = Join-Path $artifactsRoot ('.package-stage-' + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($work) | Out-Null

$portableName = "LCDForge-$version-win-x64-portable"
$installerName = "LCDForge-$version-win-x64-installer"
$sourceName = "LCDForge-$version-source"
$portableRoot = Join-Path $work $portableName
$installerRoot = Join-Path $work $installerName
$sourceRoot = Join-Path $work $sourceName

try {
    Write-Host '== assemble explicit package trees ==' -ForegroundColor Cyan
    $trackedZip = Join-Path $work 'tracked.zip'
    git archive --format=zip --output=$trackedZip HEAD
    if ($LASTEXITCODE -ne 0) { throw 'git archive failed' }
    [IO.Directory]::CreateDirectory($sourceRoot) | Out-Null
    Expand-Archive -LiteralPath $trackedZip -DestinationPath $sourceRoot
    Remove-Item -LiteralPath $trackedZip -Force

    [IO.Directory]::CreateDirectory($portableRoot) | Out-Null
    Copy-Item -LiteralPath (Join-Path $repo 'target\release\lcdforge.exe') -Destination (Join-Path $portableRoot 'lcdforge.exe')
    Copy-Allowlist -SourceRoot $sourceRoot -Destination $portableRoot -Paths @(
        'lcdforge.txt', 'LICENSE', 'README.md', 'RELEASE-NOTES.md', 'SECURITY.md',
        'docs/ARCHITECTURE.md', 'docs/CONFIGURATION.md', 'docs/HARDWARE-ACCEPTANCE.md',
        'docs/PRODUCT-SPEC.md', 'docs/REFERENCE-LAYOUT.md'
    )
    Write-PackageManifest $portableRoot

    [IO.Directory]::CreateDirectory((Join-Path $installerRoot 'payload')) | Out-Null
    foreach ($file in Get-ChildItem -LiteralPath $portableRoot -File -Force -Recurse | Where-Object { $_.Name -ne 'PACKAGE-MANIFEST.txt' }) {
        $relative = Get-RelativePackagePath -Root $portableRoot -Path $file.FullName
        $target = Join-Path $installerRoot ('payload\' + $relative.Replace('/', '\'))
        [IO.Directory]::CreateDirectory((Split-Path $target -Parent)) | Out-Null
        Copy-Item -LiteralPath $file.FullName -Destination $target
    }
    foreach ($script in @('Install.ps1', 'Uninstall.ps1', 'Package.Common.ps1')) {
        Copy-Item -LiteralPath (Join-Path $sourceRoot ('scripts\' + $script)) -Destination (Join-Path $installerRoot $script)
    }
    Write-PackageManifest $installerRoot

    [IO.File]::WriteAllText((Join-Path $sourceRoot 'SOURCE-COMMIT.txt'), $head + "`n", (New-Object Text.UTF8Encoding($false)))
    Write-PackageManifest $sourceRoot

    $portableZip = Join-Path $output ($portableName + '.zip')
    $installerZip = Join-Path $output ($installerName + '.zip')
    $sourceZip = Join-Path $output ($sourceName + '.zip')
    Write-DeterministicZip -Root $portableRoot -Archive $portableZip -Prefix $portableName
    Write-DeterministicZip -Root $installerRoot -Archive $installerZip -Prefix $installerName
    Write-DeterministicZip -Root $sourceRoot -Archive $sourceZip -Prefix $sourceName

    $archives = @($installerZip, $portableZip, $sourceZip)
    $archives = @($archives | Sort-Object { [IO.Path]::GetFileName($_) })
    $sums = @()
    foreach ($archive in $archives) {
        $hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        $size = (Get-Item -LiteralPath $archive).Length
        $sums += "{0}`t{1}`t{2}" -f $hash, $size, [IO.Path]::GetFileName($archive)
    }
    [IO.File]::WriteAllLines((Join-Path $output 'SHA256SUMS.txt'), $sums, (New-Object Text.UTF8Encoding($false)))

    Write-Host '== clean extraction and lifecycle tests ==' -ForegroundColor Cyan
    & (Join-Path $PSScriptRoot 'Package-Test.ps1') -ArtifactDir $output -ExpectedCommit $head
    if ($LASTEXITCODE -ne 0) { throw 'package tests failed' }
}
finally {
    if ([IO.Directory]::Exists($work)) { Remove-Item -LiteralPath $work -Recurse -Force }
}

if ((git status --porcelain).Count -ne 0 -or (git rev-parse HEAD).Trim() -ne $head) { throw 'source changed during package build' }
Write-Host "Provisional release artifacts: $output" -ForegroundColor Green
