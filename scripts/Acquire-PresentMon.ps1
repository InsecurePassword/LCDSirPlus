<#
.SYNOPSIS
Acquires or verifies the pinned PresentMon distribution.

.DESCRIPTION
Validates the tracked manifest, then downloads and verifies PresentMon v2.5.1 and its licenses by default. The default run writes the binary and license artifacts under third_party/PresentMon; verification creates no artifacts.

.PARAMETER VerifyOnly
Verifies the existing binary, signature, required command-line flags, and licenses without downloading files.

.EXAMPLE
PS> .\scripts\Acquire-PresentMon.ps1
Downloads and verifies the pinned PresentMon binary and licenses when needed.
#>
#Requires -Version 5.1
[CmdletBinding()]
param([switch]$VerifyOnly)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
$repo = Split-Path $PSScriptRoot -Parent
$root = Join-Path $repo 'third_party\PresentMon'
$manifestPath = Join-Path $root 'manifest.json'
$downloadTimeoutSeconds = 120

if (-not ('LCDSirPlus.PresentMonNative' -as [type])) {
    Add-Type -TypeDefinition @'
using System.Runtime.InteropServices;

namespace LCDSirPlus {
    public static class PresentMonNative {
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        public static extern bool MoveFileEx(string existingPath, string newPath, uint flags);
    }
}
'@
}

$expected = @{
    Version = 'v2.5.1'
    Commit = '3e06c7dcb922e411bae38503b51ab501be61c37f'
    Url = 'https://github.com/GameTechDev/PresentMon/releases/download/v2.5.1/PresentMon-2.5.1-x64.exe'
    Size = [uint64]956768
    Hash = '9bec3083069f58f911e6a512f4806db51a27bd096103087bc1d05ef54c80a191'
    Signer = 'Intel Corporation'
    Flags = @(
        '--process_id',
        '--output_stdout',
        '--no_console_stats',
        '--terminate_on_proc_exit',
        '--session_name',
        '--stop_existing_session',
        '--v2_metrics',
        '--exclude_dropped'
    )
}

function Assert-FileIdentity {
    param([string]$Path, [uint64]$Size, [string]$Hash, [string]$Label)
    if (-not [IO.File]::Exists($Path)) { throw "$Label is missing: $Path" }
    $actualSize = [uint64](Get-Item -LiteralPath $Path).Length
    if ($actualSize -ne $Size) { throw "$Label size mismatch: expected $Size, got $actualSize" }
    $actualHash = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -cne $Hash) { throw "$Label SHA256 mismatch: expected $Hash, got $actualHash" }
}

function Assert-PresentMonSignature {
    param([string]$Path, [string]$Signer)
    $signature = Get-AuthenticodeSignature -FilePath $Path
    if ($signature.Status -ne [Management.Automation.SignatureStatus]::Valid) {
        throw "PresentMon Authenticode signature is not valid: $($signature.Status) ($($signature.StatusMessage))"
    }
    $certificate = $signature.SignerCertificate
    $simpleName = if ($null -eq $certificate) { $null } else { $certificate.GetNameInfo([Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false) }
    if ($simpleName -cne $Signer) {
        throw "PresentMon Authenticode signer mismatch: expected $Signer, got $simpleName"
    }
}

function Assert-PresentMonHelp {
    param([string]$Path, [string[]]$Flags)
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = $Path
    $start.Arguments = '--help'
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($start)
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $output = $stdout.Result + $stderr.Result
    }
    finally { $process.Dispose() }
    foreach ($flag in $Flags) {
        if ($output.IndexOf($flag, [StringComparison]::Ordinal) -lt 0) {
            throw "PresentMon --help does not advertise required flag $flag"
        }
    }
}

function Publish-AtomicFile {
    param([string]$Temporary, [string]$Destination)
    $replaceExisting = 0x1
    $writeThrough = 0x8
    if (-not [LCDSirPlus.PresentMonNative]::MoveFileEx($Temporary, $Destination, $replaceExisting -bor $writeThrough)) {
        $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
        throw "Atomic PresentMon cache replacement failed ($errorCode): $Destination"
    }
}

if (-not [IO.File]::Exists($manifestPath)) { throw "PresentMon manifest is missing: $manifestPath" }
$manifest = [IO.File]::ReadAllText($manifestPath, [Text.Encoding]::UTF8) | ConvertFrom-Json
if ($manifest.name -cne 'PresentMon' -or $manifest.version -cne $expected.Version -or $manifest.commit -cne $expected.Commit) {
    throw 'PresentMon manifest version or commit does not match the pinned contract'
}
if ($manifest.artifact.file -cne 'PresentMon.exe' -or $manifest.artifact.url -cne $expected.Url -or
    [uint64]$manifest.artifact.size -ne $expected.Size -or $manifest.artifact.sha256 -cne $expected.Hash -or
    $manifest.artifact.authenticodeSigner -cne $expected.Signer) {
    throw 'PresentMon manifest artifact URL, size, hash, or signer does not match the pinned contract'
}
$flags = @($manifest.requiredFlags)
if ($flags.Count -ne $expected.Flags.Count) { throw 'PresentMon manifest required-flag inventory mismatch' }
for ($index = 0; $index -lt $flags.Count; $index++) {
    if ($flags[$index] -cne $expected.Flags[$index]) { throw 'PresentMon manifest required-flag inventory mismatch' }
}

$expectedLicenses = @{
    'LICENSE.txt' = @{
        File = 'LICENSE.txt'
        Url = "https://raw.githubusercontent.com/GameTechDev/PresentMon/$($expected.Commit)/LICENSE.txt"
        Size = [uint64]1067
        Hash = '4c949341b1893c8c6ad82f7fb4eedf622cd1fd9c22a9af8f19b2dac19d1947b6'
    }
    'THIRD_PARTY.txt' = @{
        File = 'THIRD_PARTY.txt'
        Url = "https://raw.githubusercontent.com/GameTechDev/PresentMon/$($expected.Commit)/THIRD_PARTY.txt"
        Size = [uint64]6471
        Hash = 'e039937f1a2fc2eb8f24a25b4229551a5a8056026d2da878507e20269eb56267'
    }
}
$licenses = @($manifest.licenses)
if ($licenses.Count -ne $expectedLicenses.Count) { throw 'PresentMon manifest license inventory mismatch' }
$licenseNames = [string[]]@($licenses | ForEach-Object { ([string]$_.file).Replace('\', '/') })
$requiredLicenseNames = [string[]]@($expectedLicenses.Keys)
[Array]::Sort($licenseNames, [StringComparer]::Ordinal)
[Array]::Sort($requiredLicenseNames, [StringComparer]::Ordinal)
for ($index = 0; $index -lt $licenseNames.Count; $index++) {
    if ($licenseNames[$index] -cne $requiredLicenseNames[$index]) { throw 'PresentMon manifest license inventory mismatch' }
}
foreach ($license in $licenses) {
    $normalized = ([string]$license.file).Replace('\', '/')
    $contract = $expectedLicenses[$normalized]
    if ($normalized -cne $license.file -or $normalized -cne $contract.File -or $license.url -cne $contract.Url -or
        [uint64]$license.size -ne $contract.Size -or $license.sha256 -cne $contract.Hash) {
        throw "PresentMon $($license.file) metadata does not match the pinned contract"
    }
}

$binary = Join-Path $root 'PresentMon.exe'
if ($VerifyOnly) {
    Assert-FileIdentity $binary $expected.Size $expected.Hash 'PresentMon binary'
    Assert-PresentMonSignature $binary $expected.Signer
    Assert-PresentMonHelp $binary $expected.Flags
    foreach ($license in $licenses) {
        Assert-FileIdentity (Join-Path $root $license.file) ([uint64]$license.size) $license.sha256 "PresentMon $($license.file)"
    }
    Write-Host 'PresentMon v2.5.1 cache and licenses verified.' -ForegroundColor Green
    return
}

[IO.Directory]::CreateDirectory($root) | Out-Null
$temporary = New-Object 'Collections.Generic.List[string]'
try {
    $binaryTemp = Join-Path $root ('.PresentMon.' + [Guid]::NewGuid().ToString('N') + '.download.exe')
    [void]$temporary.Add($binaryTemp)
    Invoke-WebRequest -Uri $manifest.artifact.url -OutFile $binaryTemp -UseBasicParsing -TimeoutSec $downloadTimeoutSeconds
    Assert-FileIdentity $binaryTemp $expected.Size $expected.Hash 'Downloaded PresentMon binary'
    Assert-PresentMonSignature $binaryTemp $expected.Signer
    Assert-PresentMonHelp $binaryTemp $expected.Flags

    $licenseTemps = @()
    foreach ($license in $licenses) {
        $temp = Join-Path $root ('.' + $license.file + '.' + [Guid]::NewGuid().ToString('N') + '.tmp')
        [void]$temporary.Add($temp)
        Invoke-WebRequest -Uri $license.url -OutFile $temp -UseBasicParsing -TimeoutSec $downloadTimeoutSeconds
        Assert-FileIdentity $temp ([uint64]$license.size) $license.sha256 "Downloaded PresentMon $($license.file)"
        $licenseTemps += [pscustomobject]@{ Temporary = $temp; Destination = Join-Path $root $license.file }
    }
    foreach ($item in $licenseTemps) { Publish-AtomicFile $item.Temporary $item.Destination }
    Publish-AtomicFile $binaryTemp $binary
    & $PSCommandPath -VerifyOnly
}
finally {
    foreach ($path in $temporary) {
        if ([IO.File]::Exists($path)) { Remove-Item -LiteralPath $path -Force }
    }
}
