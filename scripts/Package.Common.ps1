Set-StrictMode -Version 2.0

if (-not ('LCDSirPlus.PackageNative' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace LCDSirPlus {
    [StructLayout(LayoutKind.Sequential)]
    public struct ByHandleFileInformation {
        public uint FileAttributes;
        public System.Runtime.InteropServices.ComTypes.FILETIME CreationTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastAccessTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastWriteTime;
        public uint VolumeSerialNumber;
        public uint FileSizeHigh;
        public uint FileSizeLow;
        public uint NumberOfLinks;
        public uint FileIndexHigh;
        public uint FileIndexLow;
    }

    public static class PackageNative {
        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern bool GetFileInformationByHandle(
            SafeFileHandle handle,
            out ByHandleFileInformation information);
    }
}
'@
}

function Get-NormalizedFullPath {
    param([Parameter(Mandatory = $true)][string]$Path)
    return [IO.Path]::GetFullPath($Path).TrimEnd('\')
}

function Get-OrdinalSorted {
    param([string[]]$Values)
    $copy = [string[]]@($Values)
    [Array]::Sort($copy, [StringComparer]::Ordinal)
    return $copy
}

function Assert-SafeRelativePath {
    param([Parameter(Mandatory = $true)][string]$Path)
    if ([string]::IsNullOrWhiteSpace($Path) -or [IO.Path]::IsPathRooted($Path) -or $Path.Contains('\') -or $Path.Contains(':')) {
        throw "unsafe package member path"
    }
    $parts = $Path.Split('/')
    foreach ($part in $parts) {
        if ([string]::IsNullOrWhiteSpace($part) -or $part -eq '.' -or $part -eq '..' -or $part.EndsWith('.') -or $part.EndsWith(' ')) {
            throw "unsafe package member path"
        }
        if ($part.IndexOfAny([IO.Path]::GetInvalidFileNameChars()) -ge 0) {
            throw "unsafe package member path"
        }
        $base = $part.Split('.')[0]
        if ($base -match '^(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$') {
            throw "reserved package member name"
        }
    }
}

function Get-RelativePackagePath {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Path
    )
    $rootFull = (Get-NormalizedFullPath $Root) + '\'
    $pathFull = Get-NormalizedFullPath $Path
    if (-not $pathFull.StartsWith($rootFull, [StringComparison]::OrdinalIgnoreCase)) {
        throw "package member escaped root"
    }
    return $pathFull.Substring($rootFull.Length).Replace('\', '/')
}

function Assert-RegularSingleLinkFile {
    param([Parameter(Mandatory = $true)][string]$Path)
    $stream = $null
    try {
        $share = [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
        $stream = New-Object IO.FileStream($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, $share)
        $information = New-Object LCDSirPlus.ByHandleFileInformation
        if (-not [LCDSirPlus.PackageNative]::GetFileInformationByHandle($stream.SafeFileHandle, [ref]$information)) {
            throw "file identity unavailable"
        }
        $directory = 0x10
        $reparse = 0x400
        if (($information.FileAttributes -band ($directory -bor $reparse)) -ne 0 -or $information.NumberOfLinks -ne 1) {
            throw "file must be regular, non-reparse, and single-linked"
        }
        return [pscustomobject]@{
            Volume = $information.VolumeSerialNumber
            Index = ([uint64]$information.FileIndexHigh -shl 32) -bor [uint64]$information.FileIndexLow
            Size = ([uint64]$information.FileSizeHigh -shl 32) -bor [uint64]$information.FileSizeLow
        }
    }
    finally {
        if ($null -ne $stream) { $stream.Dispose() }
    }
}

function Assert-SafeLocalDirectory {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [switch]$Create,
        [switch]$AllowMissing
    )
    $full = Get-NormalizedFullPath $Path
    if (-not [IO.Path]::IsPathRooted($full) -or $full.StartsWith('\\')) {
        throw "directory must be an absolute local path"
    }
    $root = [IO.Path]::GetPathRoot($full)
    if ([string]::IsNullOrEmpty($root) -or $full.TrimEnd('\') -eq $root.TrimEnd('\')) {
        throw "dangerous directory root refused"
    }
    $drive = New-Object IO.DriveInfo($root)
    if ($drive.DriveType -ne [IO.DriveType]::Fixed) {
        throw "directory must be on a fixed local volume"
    }
    $current = $root
    $relative = $full.Substring($root.Length)
    foreach ($part in $relative.Split([char]'\')) {
        if ([string]::IsNullOrEmpty($part)) { continue }
        $current = Join-Path $current $part
        if (-not [IO.Directory]::Exists($current)) {
            if ($AllowMissing) { break }
            if (-not $Create) { throw "directory does not exist" }
            [IO.Directory]::CreateDirectory($current) | Out-Null
        }
        $attributes = [IO.File]::GetAttributes($current)
        if (($attributes -band [IO.FileAttributes]::Directory) -eq 0 -or ($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "directory boundary is not trusted"
        }
    }
    return $full
}

function New-ReleaseOutputTransaction {
    param([Parameter(Mandatory = $true)][string]$Output)
    $outputFull = Get-NormalizedFullPath $Output
    $parent = Assert-SafeLocalDirectory -Path (Split-Path $outputFull -Parent) -Create
    $leaf = Split-Path $outputFull -Leaf
    $stage = Join-Path $parent ('.' + $leaf + '.stage.' + [Guid]::NewGuid().ToString('N'))
    try {
        [IO.Directory]::CreateDirectory($stage) | Out-Null
        $stage = Assert-SafeLocalDirectory -Path $stage
        if (-not (Get-NormalizedFullPath (Split-Path $stage -Parent)).Equals($parent, [StringComparison]::OrdinalIgnoreCase) -or
            @(Get-ChildItem -LiteralPath $stage -Force).Count -ne 0) {
            throw "release stage must be an empty sibling of output"
        }
        $hadOutput = [IO.Directory]::Exists($outputFull)
        if ($hadOutput) {
            Assert-SafeLocalDirectory -Path $outputFull | Out-Null
        }
        elseif ([IO.File]::Exists($outputFull)) {
            throw "release output must be a directory"
        }
        return [pscustomobject]@{ Output = $outputFull; Stage = $stage; Prior = $null; HadOutput = $hadOutput }
    }
    catch {
        if ([IO.Directory]::Exists($stage)) { [IO.Directory]::Delete($stage, $true) }
        throw
    }
}

function Complete-ReleaseOutputTransaction {
    param(
        [Parameter(Mandatory = $true)][object]$Transaction,
        [ValidateSet('None', 'BeforeMove', 'AfterMove')][string]$TestFailAt = 'None'
    )
    if ($TestFailAt -ne 'None' -and $env:LCDSIRPLUS_PACKAGE_TEST -cne '1') {
        throw "release fault injection requires guarded package test mode"
    }
    $stage = Assert-SafeLocalDirectory -Path $Transaction.Stage
    $output = Get-NormalizedFullPath $Transaction.Output
    $parent = Assert-SafeLocalDirectory -Path (Split-Path $output -Parent)
    if (-not (Get-NormalizedFullPath (Split-Path $stage -Parent)).Equals($parent, [StringComparison]::OrdinalIgnoreCase)) {
        throw "release stage must be a sibling of output"
    }
    $prior = $null
    if ([bool]$Transaction.HadOutput) {
        Assert-SafeLocalDirectory -Path $output | Out-Null
        $prior = Join-Path $parent ('.' + (Split-Path $output -Leaf) + '.prior.' + [Guid]::NewGuid().ToString('N'))
    }
    elseif ([IO.Directory]::Exists($output) -or [IO.File]::Exists($output)) {
        throw "release output appeared before promotion"
    }

    $movedPrior = $false
    try {
        if ($TestFailAt -eq 'BeforeMove') { throw "injected release failure before canonical move" }
        if ($null -ne $prior) {
            $Transaction.Prior = $prior
            [IO.Directory]::Move($output, $prior)
            $movedPrior = $true
        }
        if ($TestFailAt -eq 'AfterMove') { throw "injected release failure after canonical move" }
        [IO.Directory]::Move($stage, $output)
    }
    catch {
        $promotionFailure = $_
        if ($movedPrior) {
            try {
                if ([IO.Directory]::Exists($output) -or [IO.File]::Exists($output)) { throw "canonical output path is occupied" }
                [IO.Directory]::Move($prior, $output)
                $Transaction.Prior = $null
            }
            catch {
                throw "release promotion failed and prior restoration failed; recover by moving '$prior' to '$output'. Promotion: $promotionFailure Restoration: $_"
            }
        }
        throw $promotionFailure
    }

    if ($null -ne $prior -and [IO.Directory]::Exists($prior)) {
        try {
            Assert-SafeTree $prior
            Remove-Item -LiteralPath $prior -Recurse -Force
            $Transaction.Prior = $null
        }
        catch { Write-Warning "Current release was promoted; prior-output cleanup residue retained at ${prior}: $_" }
    }
}

function Assert-SafeTree {
    param([Parameter(Mandatory = $true)][string]$Root)
    [void]@(Get-SafeTreeFiles $Root)
}

function Get-SafeTreeFiles {
    param([Parameter(Mandatory = $true)][string]$Root)
    $rootFull = Assert-SafeLocalDirectory -Path $Root
    $pending = New-Object 'Collections.Generic.Stack[string]'
    $pending.Push($rootFull)
    while ($pending.Count -gt 0) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "tree contains a reparse point"
            }
            if ($item.PSIsContainer) {
                $pending.Push($item.FullName)
            }
            else {
                Assert-RegularSingleLinkFile -Path $item.FullName | Out-Null
                Write-Output $item
            }
        }
    }
}

function Read-VerifiedManifest {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [string]$ManifestName = 'PACKAGE-MANIFEST.txt',
        [string[]]$AllowedUndeclared = @(),
        [switch]$AllowOtherFiles
    )
    $rootFull = Assert-SafeLocalDirectory -Path $Root
    $manifestPath = Join-Path $rootFull $ManifestName
    Assert-RegularSingleLinkFile -Path $manifestPath | Out-Null
    $lines = @([IO.File]::ReadAllLines($manifestPath, [Text.Encoding]::UTF8) | Where-Object { $_.Length -gt 0 })
    $entries = @()
    $names = New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
    $last = $null
    foreach ($line in $lines) {
        if ($line -notmatch '^([0-9a-f]{64})\t([0-9]+)\t(.+)$') { throw "invalid package manifest line" }
        $relative = $Matches[3]
        Assert-SafeRelativePath $relative
        if (-not $names.Add($relative)) { throw "duplicate or case-colliding package member" }
        if ($null -ne $last -and [StringComparer]::Ordinal.Compare($last, $relative) -ge 0) {
            throw "package manifest is not strictly sorted: $last then $relative"
        }
        $last = $relative
        $path = Join-Path $rootFull $relative.Replace('/', '\')
        $identity = Assert-RegularSingleLinkFile -Path $path
        if ($identity.Size -ne [uint64]$Matches[2]) { throw "package member size mismatch" }
        $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($hash -ne $Matches[1]) { throw "package member hash mismatch" }
        $entries += [pscustomobject]@{ Path = $relative; FullName = $path; Size = $identity.Size; Hash = $hash }
    }
    if ($AllowOtherFiles) { return $entries }
    $actual = New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
    $allowed = New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
    foreach ($name in $AllowedUndeclared) {
        Assert-SafeRelativePath $name
        [void]$allowed.Add($name)
    }
    foreach ($file in Get-SafeTreeFiles $rootFull) {
        $relative = Get-RelativePackagePath -Root $rootFull -Path $file.FullName
        Assert-SafeRelativePath $relative
        if (-not $actual.Add($relative)) { throw "duplicate or case-colliding package member" }
        if (-not $AllowOtherFiles -and $relative -ne $ManifestName -and -not $names.Contains($relative) -and -not $allowed.Contains($relative)) {
            throw "undeclared package member"
        }
    }
    $allowedPresent = 0
    foreach ($name in $allowed) { if ($actual.Contains($name)) { $allowedPresent++ } }
    if ((-not $AllowOtherFiles -and $actual.Count -ne $entries.Count + 1 + $allowedPresent) -or -not $actual.Contains($ManifestName)) {
        throw "package manifest inventory mismatch"
    }
    return $entries
}

function Assert-ArchiveMemberNames {
    param([Parameter(Mandatory = $true)][string[]]$Names)
    $seen = New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
    foreach ($name in $Names) {
        $candidate = $name.TrimEnd('/')
        Assert-SafeRelativePath $candidate
        if (-not $seen.Add($candidate)) { throw "duplicate or case-colliding archive member" }
    }
}

function Test-ExactProcessRunning {
    param([Parameter(Mandatory = $true)][string]$ExecutablePath)
    $expected = Get-NormalizedFullPath $ExecutablePath
    foreach ($process in [Diagnostics.Process]::GetProcesses()) {
        try {
            $actual = Get-NormalizedFullPath $process.MainModule.FileName
            if ($actual.Equals($expected, [StringComparison]::OrdinalIgnoreCase)) { return $true }
        }
        catch { }
        finally { $process.Dispose() }
    }
    return $false
}
