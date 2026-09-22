<#
.SYNOPSIS
Builds and validates the Windows release artifacts.

.DESCRIPTION
Requires a clean worktree, pinned Rust tooling, and verified third-party caches. Runs quality and package gates, reproducibly builds the application and installer, and publishes setup, portable, source, checksum, and reproducibility artifacts beneath artifacts/.

.PARAMETER OutputDir
Release output directory relative to the repository root. Defaults to .\artifacts\release and must remain beneath artifacts/; a prior release may be retained by the output transaction.

.EXAMPLE
PS> .\scripts\Build.ps1
Builds the release into .\artifacts\release.
#>
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
Assert-SafeLocalDirectory -Path $artifactsRoot -AllowMissing | Out-Null
[IO.Directory]::CreateDirectory($artifactsRoot) | Out-Null
$artifactsRoot = Assert-SafeLocalDirectory -Path $artifactsRoot
$output = [IO.Path]::GetFullPath((Join-Path $repo $OutputDir))
if ($output.Equals($artifactsRoot, [StringComparison]::OrdinalIgnoreCase) -or
    -not ($output + '\').StartsWith($artifactsRoot.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'release output must remain beneath artifacts/'
}
Assert-SafeLocalDirectory -Path $output -AllowMissing | Out-Null

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

$expectedRustcVersion = '1.97.1'
$expectedRustcCommit = '8bab26f4f68e0e26f0bb7960be334d5b520ea452'
function Select-PinnedRustToolchain {
    $rustup = Get-Command rustup -CommandType Application -ErrorAction SilentlyContinue
    $install = 'rustup toolchain install 1.97.1 --profile minimal --component clippy --component rustfmt --target x86_64-pc-windows-msvc'
    if ($null -eq $rustup) { throw "Rust 1.97.1 is required. Install it with: $install" }
    $name = [Environment]::GetEnvironmentVariable('RUSTUP_TOOLCHAIN', 'Process')
    if ([string]::IsNullOrWhiteSpace($name)) {
        $name = '1.97.1-x86_64-pc-windows-msvc'
        $installed = @(& $rustup.Source toolchain list | ForEach-Object { ($_ -split '\s+')[0] })
        if ($name -cnotin $installed) { throw "The exact Rust 1.97.1 toolchain alias is not installed. Install it with: $install" }
    }
    $verbose = (& $rustup.Source run $name rustc --version --verbose | Out-String).Replace("`r", '')
    $components = @(& $rustup.Source component list --toolchain $name --installed)
    $targets = @(& $rustup.Source target list --toolchain $name --installed)
    if ($LASTEXITCODE -ne 0 -or $verbose -notmatch "(?m)^release: $([regex]::Escape($expectedRustcVersion))$" -or
        $verbose -notmatch "(?m)^commit-hash: $expectedRustcCommit$" -or
        $verbose -notmatch '(?m)^host: x86_64-pc-windows-msvc$' -or
        @($components | Where-Object { $_ -match '^clippy-' }).Count -ne 1 -or
        @($components | Where-Object { $_ -match '^rustfmt-' }).Count -ne 1 -or
        @($targets | Where-Object { $_ -ceq 'x86_64-pc-windows-msvc' }).Count -ne 1) {
        throw "Selected Rust toolchain '$name' is not the pinned 1.97.1 provenance with required components and target. Install it with: $install"
    }
    return $name
}

$encodedRustFlagsBefore = [Environment]::GetEnvironmentVariable('CARGO_ENCODED_RUSTFLAGS', 'Process')
$rustupToolchainBefore = [Environment]::GetEnvironmentVariable('RUSTUP_TOOLCHAIN', 'Process')
$remapRoots = @(
    [pscustomobject]@{ Path = $repo; Destination = '/workspace' },
    [pscustomobject]@{ Path = $(if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }); Destination = '/cargo' },
    [pscustomobject]@{ Path = $env:USERPROFILE; Destination = '/user' },
    [pscustomobject]@{ Path = $env:HOME; Destination = '/home' },
    [pscustomobject]@{ Path = [IO.Path]::GetTempPath(); Destination = '/tmp' }
)
$seenRemaps = New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
$remapFlags = @()
foreach ($remap in $remapRoots) {
    if ([string]::IsNullOrWhiteSpace($remap.Path)) { continue }
    $source = Get-NormalizedFullPath $remap.Path
    if ($seenRemaps.Add($source)) { $remapFlags += "--remap-path-prefix=$source=$($remap.Destination)" }
}
$remapFlags += @('-C', 'link-arg=/Brepro')

$release = $null
$candidate = $null
$work = $null
try {
    $toolchain = [IO.File]::ReadAllText((Join-Path $repo 'rust-toolchain.toml'), [Text.Encoding]::UTF8).Replace("`r`n", "`n")
    if ($toolchain -cne "[toolchain]`nchannel = `"1.97.1`"`nprofile = `"minimal`"`ncomponents = [`"clippy`", `"rustfmt`"]`ntargets = [`"x86_64-pc-windows-msvc`"]`n") {
        throw 'rust-toolchain.toml does not pin the exact Rust version, minimal profile, components, and Windows MSVC target'
    }
    $env:RUSTUP_TOOLCHAIN = Select-PinnedRustToolchain
    if (@(git status --porcelain).Count -ne 0) { throw 'release build requires a clean worktree' }
    $head = (git rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or $head -notmatch '^[0-9a-f]{40}$') { throw 'cannot resolve release commit' }
    $noticeText = [IO.File]::ReadAllText((Join-Path $repo 'THIRD_PARTY_LICENSES.txt'), [Text.Encoding]::UTF8)
    if ($noticeText.IndexOf("rustc $expectedRustcVersion", [StringComparison]::Ordinal) -lt 0 -or
        $noticeText.IndexOf($expectedRustcCommit, [StringComparison]::Ordinal) -lt 0) {
        throw 'third-party notices do not identify the selected rustc version and commit'
    }
    & (Join-Path $PSScriptRoot 'Acquire-PresentMon.ps1') -VerifyOnly
    & (Join-Path $PSScriptRoot 'Acquire-InnoSetup.ps1') -VerifyOnly

    $env:CARGO_ENCODED_RUSTFLAGS = $remapFlags -join [char]0x1f
    Write-Host '== quality gate ==' -ForegroundColor Cyan
    & (Join-Path $PSScriptRoot 'Test.ps1')
    if (-not $?) { throw 'quality gate failed' }
    if (@(git status --porcelain).Count -ne 0 -or (git rev-parse HEAD).Trim() -ne $head) { throw 'source changed during quality gate' }

    $release = New-ReleaseOutputTransaction -Output $output
    $candidate = $release.Stage
    if ($null -ne $release.Prior) { Write-Host "Prior release retained at $($release.Prior)" -ForegroundColor Yellow }

    $work = Join-Path $artifactsRoot ('.package-stage-' + [Guid]::NewGuid().ToString('N'))
    [IO.Directory]::CreateDirectory($work) | Out-Null
    Assert-SafeLocalDirectory -Path $work | Out-Null

    $portableName = "LCDSirPlus-$version-win-x64-portable"
    $setupName = "LCDSirPlus-$version-win-x64-setup"
    $sourceName = "LCDSirPlus-$version-source"
    $checksumName = "LCDSirPlus-$version-SHA256SUMS.txt"
    $portableRoot = Join-Path $work $portableName
    $innoPayload = Join-Path $work 'inno-payload'
    $sourceRoot = Join-Path $work $sourceName
    $verifiedBuild = Join-Path $work 'verified-build'

    try {
        Write-Host '== assemble explicit package trees ==' -ForegroundColor Cyan
        $trackedZip = Join-Path $work 'tracked.zip'
        git archive --format=zip --output=$trackedZip HEAD
        if ($LASTEXITCODE -ne 0) { throw 'git archive failed' }
        [IO.Directory]::CreateDirectory($sourceRoot) | Out-Null
        Expand-Archive -LiteralPath $trackedZip -DestinationPath $sourceRoot
        Remove-Item -LiteralPath $trackedZip -Force
        foreach ($license in @('LICENSE.txt', 'THIRD_PARTY.txt')) {
            Copy-Item -LiteralPath (Join-Path $repo ('third_party\PresentMon\' + $license)) -Destination (Join-Path $sourceRoot ('third_party\PresentMon\' + $license)) -Force
        }

        [IO.Directory]::CreateDirectory($verifiedBuild) | Out-Null
        & (Join-Path $sourceRoot 'scripts\Test-ReproducibleBuild.ps1') -SourceRoot $sourceRoot -OutputDir $verifiedBuild -SourceCommit $head
        if ($LASTEXITCODE -ne 0) { throw 'reproducible release build failed' }
        Copy-Item -LiteralPath (Join-Path $verifiedBuild 'REPRODUCIBILITY.json') -Destination (Join-Path $candidate 'REPRODUCIBILITY.json')

        [IO.Directory]::CreateDirectory($portableRoot) | Out-Null
        Copy-Item -LiteralPath (Join-Path $verifiedBuild 'LCDSirPlus.exe') -Destination (Join-Path $portableRoot 'LCDSirPlus.exe')
        Copy-Item -LiteralPath (Join-Path $verifiedBuild 'lcdsirplus.txt') -Destination (Join-Path $portableRoot 'lcdsirplus.txt')
        Copy-Item -LiteralPath (Join-Path $repo 'third_party\PresentMon\PresentMon.exe') -Destination (Join-Path $portableRoot 'PresentMon.exe')
        [IO.File]::WriteAllText((Join-Path $portableRoot 'SOURCE-COMMIT.txt'), $head + "`n", [Text.Encoding]::ASCII)
        Copy-Allowlist -SourceRoot $sourceRoot -Destination $portableRoot -Paths @(
            'LICENSE', 'THIRD_PARTY_LICENSES.txt', 'README.md', 'modules.md', 'RELEASE-NOTES.md', 'SECURITY.md',
            'docs/CONFIGURATION.md', 'docs/INSTRUCTION-MANUAL.md',
            'docs/LCDSirPlus-Instruction-Manual.pdf'
        )
        $presentMonLicenses = Join-Path $portableRoot 'licenses\PresentMon'
        [IO.Directory]::CreateDirectory($presentMonLicenses) | Out-Null
        foreach ($license in @('LICENSE.txt', 'THIRD_PARTY.txt')) {
            Copy-Item -LiteralPath (Join-Path $repo ('third_party\PresentMon\' + $license)) -Destination (Join-Path $presentMonLicenses $license)
        }
        Write-PackageManifest $portableRoot

        [IO.Directory]::CreateDirectory($innoPayload) | Out-Null
        Copy-Item -LiteralPath (Join-Path $verifiedBuild 'LCDSirPlus.exe') -Destination (Join-Path $innoPayload 'LCDSirPlus.exe')
        Copy-Item -LiteralPath (Join-Path $verifiedBuild 'lcdsirplus.txt') -Destination (Join-Path $innoPayload 'lcdsirplus.default.txt')
        Copy-Item -LiteralPath (Join-Path $repo 'third_party\PresentMon\PresentMon.exe') -Destination (Join-Path $innoPayload 'PresentMon.exe')
        [IO.File]::WriteAllText((Join-Path $innoPayload 'lcdsirplus.layout'), 'installed-v1', (New-Object Text.UTF8Encoding($false)))
        [IO.File]::WriteAllText((Join-Path $innoPayload 'SOURCE-COMMIT.txt'), $head + "`n", [Text.Encoding]::ASCII)
        Copy-Allowlist -SourceRoot $sourceRoot -Destination $innoPayload -Paths @(
            'LICENSE', 'THIRD_PARTY_LICENSES.txt', 'README.md', 'modules.md', 'RELEASE-NOTES.md', 'SECURITY.md',
            'docs/CONFIGURATION.md', 'docs/INSTRUCTION-MANUAL.md',
            'docs/LCDSirPlus-Instruction-Manual.pdf'
        )
        $innoPresentMonLicenses = Join-Path $innoPayload 'licenses\PresentMon'
        [IO.Directory]::CreateDirectory($innoPresentMonLicenses) | Out-Null
        foreach ($license in @('LICENSE.txt', 'THIRD_PARTY.txt')) {
            Copy-Item -LiteralPath (Join-Path $repo ('third_party\PresentMon\' + $license)) -Destination (Join-Path $innoPresentMonLicenses $license)
        }

        [IO.File]::WriteAllText((Join-Path $sourceRoot 'SOURCE-COMMIT.txt'), $head + "`n", [Text.Encoding]::ASCII)
        Write-PackageManifest $sourceRoot

        $portableZip = Join-Path $candidate ($portableName + '.zip')
        $sourceZip = Join-Path $candidate ($sourceName + '.zip')
        Write-DeterministicZip -Root $portableRoot -Archive $portableZip -Prefix $portableName
        Write-DeterministicZip -Root $sourceRoot -Archive $sourceZip -Prefix $sourceName

        Write-Host '== compile deterministic Inno setup twice ==' -ForegroundColor Cyan
        $innoRoot = Join-Path $work ('inno-compiler-' + [Guid]::NewGuid().ToString('N'))
        & (Join-Path $PSScriptRoot 'Acquire-InnoSetup.ps1') -ExtractVerified -OutputDir $innoRoot
        $iscc = Join-Path $innoRoot 'ISCC.exe'
        $iss = Join-Path $sourceRoot 'packaging\LCDSirPlus.iss'
        $compileA = Join-Path $work 'inno-output-a'
        $compileB = Join-Path $work 'inno-output-b'
        [IO.Directory]::CreateDirectory($compileA) | Out-Null
        [IO.Directory]::CreateDirectory($compileB) | Out-Null
        foreach ($compileOutput in @($compileA, $compileB)) {
            & $iscc /Qp ("/DPayloadRoot=$innoPayload") ("/DSourceIdentity=$head") ("/O$compileOutput") ("/F$setupName") $iss
            if ($LASTEXITCODE -ne 0) { throw 'Inno Setup compilation failed' }
        }
        $compiledA = Join-Path $compileA ($setupName + '.exe')
        $compiledB = Join-Path $compileB ($setupName + '.exe')
        $setupHash = (Get-FileHash -LiteralPath $compiledA -Algorithm SHA256).Hash
        if ($setupHash -cne (Get-FileHash -LiteralPath $compiledB -Algorithm SHA256).Hash -or
            (Get-Item -LiteralPath $compiledA).Length -ne (Get-Item -LiteralPath $compiledB).Length) {
            throw 'Inno Setup output is not byte-identical across two compilations'
        }
        $setupExe = Join-Path $candidate ($setupName + '.exe')
        Copy-Item -LiteralPath $compiledA -Destination $setupExe

        $reproducibility = Join-Path $candidate 'REPRODUCIBILITY.json'
        $releaseFiles = @($setupExe, $portableZip, $sourceZip, $reproducibility)
        $releaseFiles = @($releaseFiles | Sort-Object { [IO.Path]::GetFileName($_) })
        $sums = @()
        foreach ($releaseFile in $releaseFiles) {
            $hash = (Get-FileHash -LiteralPath $releaseFile -Algorithm SHA256).Hash.ToLowerInvariant()
            $size = (Get-Item -LiteralPath $releaseFile).Length
            $sums += "{0}`t{1}`t{2}" -f $hash, $size, [IO.Path]::GetFileName($releaseFile)
        }
        [IO.File]::WriteAllLines((Join-Path $candidate $checksumName), $sums, (New-Object Text.UTF8Encoding($false)))

        Write-Host '== clean extraction and static setup tests ==' -ForegroundColor Cyan
        & (Join-Path $PSScriptRoot 'Package-Test.ps1') -ArtifactDir $candidate -ExpectedCommit $head `
            -ExpectedSetupSha256 $setupHash.ToLowerInvariant() -ExpectedSourceIdentity $head -InstallerPayloadDir $innoPayload
        if ($LASTEXITCODE -ne 0) { throw 'package tests failed' }
    }
    finally {
        if ($null -ne $work -and [IO.Directory]::Exists($work)) {
            Assert-SafeTree $work
            Remove-Item -LiteralPath $work -Recurse -Force
        }
    }

    if (@(git status --porcelain).Count -ne 0 -or (git rev-parse HEAD).Trim() -ne $head) { throw 'source changed during package build' }
    Complete-ReleaseOutputTransaction -Transaction $release
    Write-Host "Release artifacts: $output" -ForegroundColor Green
}
catch {
    if ($null -ne $release -and $null -ne $release.Prior) {
        Write-Warning "Release failed after staging began; prior release retained at $($release.Prior)"
    }
    throw
}
finally {
    if ($null -eq $encodedRustFlagsBefore) { Remove-Item Env:\CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue }
    else { $env:CARGO_ENCODED_RUSTFLAGS = $encodedRustFlagsBefore }
    if ($null -eq $rustupToolchainBefore) { Remove-Item Env:\RUSTUP_TOOLCHAIN -ErrorAction SilentlyContinue }
    else { $env:RUSTUP_TOOLCHAIN = $rustupToolchainBefore }
    if ($null -ne $release -and [IO.Directory]::Exists($release.Stage)) {
        Assert-SafeTree $release.Stage
        Remove-Item -LiteralPath $release.Stage -Recurse -Force
    }
}
