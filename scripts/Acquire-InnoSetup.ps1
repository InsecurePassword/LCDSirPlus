<#
.SYNOPSIS
Acquires or verifies the pinned Inno Setup compiler installer.

.DESCRIPTION
Validates the tracked manifest and license, then downloads and verifies Inno Setup 6.7.3 by default. The default run creates the installer cache under third_party/InnoSetup; verification creates no artifacts, while extraction creates a new portable compiler directory.

.PARAMETER VerifyOnly
Verifies the existing installer cache without downloading or creating artifacts.

.PARAMETER ExtractVerified
Verifies the cached installer and extracts a portable compiler to OutputDir. Cannot be combined with VerifyOnly.

.PARAMETER OutputDir
New extraction directory used with ExtractVerified. Relative paths are resolved from the repository root, and the directory must not already exist.

.EXAMPLE
PS> .\scripts\Acquire-InnoSetup.ps1
Downloads the pinned installer into its repository cache when it is not already present and verified.
#>
#Requires -Version 5.1
[CmdletBinding()]
param(
    [switch]$VerifyOnly,
    [switch]$ExtractVerified,
    [string]$OutputDir
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
$repo = Split-Path $PSScriptRoot -Parent
$metadataRoot = Join-Path $repo 'third_party\InnoSetup'
$manifestPath = Join-Path $metadataRoot 'manifest.json'
$cacheRoot = Join-Path $metadataRoot 'cache\6.7.3'
$downloadTimeoutSeconds = 120

$expected = @{
    Version = '6.7.3'
    Tag = 'is-6_7_3'
    File = 'innosetup-6.7.3.exe'
    Url = 'https://github.com/jrsoftware/issrc/releases/download/is-6_7_3/innosetup-6.7.3.exe'
    Size = [uint64]10592232
    Hash = '9c73c3bae7ed48d44112a0f48e66742c00090bdb5bef71d9d3c056c66e97b732'
    Signer = 'Pyrsys B.V.'
    Subject = 'CN=Pyrsys B.V., O=Pyrsys B.V., S=Noord-Holland, C=NL'
    Thumbprint = 'E0AB19C8D38CBF9C44709925122A7A02F8C70CB7'
    IsccSize = [uint64]1456272
    IsccHash = '0a8757031b33777e4c9cbffee40f11a5062b36d25cbe144c1db73b6102b80ad7'
    Banner = 'Inno Setup 6 Command-Line Compiler'
    TrackedLicenseSize = [uint64]1491
    TrackedLicenseHash = '0c81595601bce47eeef8d865d5da7f9ca2c6a12235b7482b29f5ab23ed02ee5a'
    InstalledLicenseSize = [uint64]1521
    InstalledLicenseHash = '2e5346868c2a18434489824e11d65c3031620f792fefc415d05f19cd441abf5c'
}

function Assert-FileIdentity {
    param([string]$Path, [uint64]$Size, [string]$Hash, [string]$Label)
    if (-not [IO.File]::Exists($Path)) { throw "$Label is missing: $Path" }
    $actualSize = [uint64](Get-Item -LiteralPath $Path).Length
    if ($actualSize -ne $Size) { throw "$Label size mismatch: expected $Size, got $actualSize" }
    $actualHash = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -cne $Hash) { throw "$Label SHA256 mismatch: expected $Hash, got $actualHash" }
}

function Assert-InstallerSignature {
    param([string]$Path)
    $signature = Get-AuthenticodeSignature -FilePath $Path
    $certificate = $signature.SignerCertificate
    $simpleName = if ($null -eq $certificate) { $null } else { $certificate.GetNameInfo([Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false) }
    $subject = if ($null -eq $certificate) { $null } else { $certificate.Subject }
    $thumbprint = if ($null -eq $certificate) { $null } else { $certificate.Thumbprint }
    if ($signature.Status -ne [Management.Automation.SignatureStatus]::Valid -or $null -eq $certificate -or
        $simpleName -cne $expected.Signer -or $subject -cne $expected.Subject -or
        $thumbprint -cne $expected.Thumbprint) {
        throw "Inno Setup Authenticode identity mismatch: $($signature.Status), $simpleName, $subject, $thumbprint"
    }
}

function Assert-IsccBanner {
    param([string]$Path)
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = $Path
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($start)
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $outputText = $stdout.Result + $stderr.Result
    }
    finally { $process.Dispose() }
    $lines = @($outputText -split "`r?`n" | Where-Object { $_.Length -gt 0 })
    if (-not $lines.Contains($expected.Banner)) { throw 'ISCC banner mismatch' }
}

if (-not [IO.File]::Exists($manifestPath)) { throw "Inno Setup manifest is missing: $manifestPath" }
$manifest = [IO.File]::ReadAllText($manifestPath, [Text.Encoding]::UTF8) | ConvertFrom-Json
if ($manifest.name -cne 'Inno Setup' -or $manifest.version -cne $expected.Version -or $manifest.tag -cne $expected.Tag -or
    $manifest.installer.file -cne $expected.File -or $manifest.installer.url -cne $expected.Url -or
    [uint64]$manifest.installer.size -ne $expected.Size -or $manifest.installer.sha256 -cne $expected.Hash -or
    $manifest.installer.authenticodeSimpleName -cne $expected.Signer -or
    $manifest.installer.authenticodeSubject -cne $expected.Subject -or
    $manifest.installer.authenticodeThumbprint -cne $expected.Thumbprint -or
    $manifest.compiler.file -cne 'ISCC.exe' -or [uint64]$manifest.compiler.size -ne $expected.IsccSize -or
    $manifest.compiler.sha256 -cne $expected.IsccHash -or $manifest.compiler.banner -cne $expected.Banner -or
    $manifest.license.file -cne 'LICENSE.txt' -or $manifest.license.installedFile -cne 'license.txt' -or
    [uint64]$manifest.license.size -ne $expected.TrackedLicenseSize -or
    $manifest.license.sha256 -cne $expected.TrackedLicenseHash -or
    [uint64]$manifest.license.installedSize -ne $expected.InstalledLicenseSize -or
    $manifest.license.installedSha256 -cne $expected.InstalledLicenseHash) {
    throw 'Inno Setup manifest does not match the pinned contract'
}
Assert-FileIdentity (Join-Path $metadataRoot 'LICENSE.txt') $expected.TrackedLicenseSize $expected.TrackedLicenseHash 'Tracked Inno Setup license'

function Assert-PortableCompiler {
    param([string]$Root)
    $iscc = Join-Path $Root 'ISCC.exe'
    Assert-FileIdentity $iscc $expected.IsccSize $expected.IsccHash 'ISCC compiler'
    Assert-IsccBanner $iscc
    Assert-FileIdentity (Join-Path $Root 'license.txt') $expected.InstalledLicenseSize $expected.InstalledLicenseHash 'Installed Inno Setup license'
}

if ($VerifyOnly -and $ExtractVerified) { throw '-VerifyOnly and -ExtractVerified are mutually exclusive' }
$installer = Join-Path $cacheRoot $expected.File

if ($VerifyOnly -or $ExtractVerified) {
    Assert-FileIdentity $installer $expected.Size $expected.Hash 'Cached Inno Setup installer'
    Assert-InstallerSignature $installer
    if ($VerifyOnly) {
        Write-Host "Inno Setup 6.7.3 installer cache verified: $installer" -ForegroundColor Green
        return
    }

    if ([string]::IsNullOrWhiteSpace($OutputDir)) { throw '-ExtractVerified requires OutputDir' }
    if (-not [IO.Path]::IsPathRooted($OutputDir)) { $OutputDir = Join-Path $repo $OutputDir }
    $output = [IO.Path]::GetFullPath($OutputDir).TrimEnd('\')
    if ([IO.Directory]::Exists($output) -or [IO.File]::Exists($output)) {
        throw 'verified Inno Setup extraction output must not already exist'
    }
    [IO.Directory]::CreateDirectory((Split-Path $output -Parent)) | Out-Null
    $succeeded = $false
    try {
        $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', '/PORTABLE=1', ('/DIR="' + $output + '"'))
        $process = Start-Process -FilePath $installer -ArgumentList $arguments -Wait -PassThru
        try {
            if ($process.ExitCode -ne 0) { throw "Inno Setup portable extraction failed with exit code $($process.ExitCode)" }
        }
        finally { $process.Dispose() }
        Assert-PortableCompiler $output
        $succeeded = $true
        Write-Host "Fresh Inno Setup 6.7.3 compiler verified: $output" -ForegroundColor Green
    }
    finally {
        if (-not $succeeded -and [IO.Directory]::Exists($output)) { Remove-Item -LiteralPath $output -Recurse -Force }
    }
    return
}

if (-not [string]::IsNullOrWhiteSpace($OutputDir)) { throw 'OutputDir is valid only with -ExtractVerified' }
[IO.Directory]::CreateDirectory($cacheRoot) | Out-Null
if ([IO.File]::Exists($installer)) {
    Assert-FileIdentity $installer $expected.Size $expected.Hash 'Cached Inno Setup installer'
    Assert-InstallerSignature $installer
    Write-Host "Inno Setup 6.7.3 installer already acquired and verified: $installer" -ForegroundColor Green
    return
}
$download = Join-Path $cacheRoot ('.innosetup-6.7.3-' + [Guid]::NewGuid().ToString('N') + '.exe')
try {
    Invoke-WebRequest -Uri $expected.Url -OutFile $download -UseBasicParsing -TimeoutSec $downloadTimeoutSeconds
    Assert-FileIdentity $download $expected.Size $expected.Hash 'Downloaded Inno Setup installer'
    Assert-InstallerSignature $download
    [IO.File]::Move($download, $installer)
    Assert-FileIdentity $installer $expected.Size $expected.Hash 'Cached Inno Setup installer'
    Assert-InstallerSignature $installer
    Write-Host "Inno Setup 6.7.3 installer acquired and verified: $installer" -ForegroundColor Green
}
finally {
    if ([IO.File]::Exists($download)) { Remove-Item -LiteralPath $download -Force }
}
