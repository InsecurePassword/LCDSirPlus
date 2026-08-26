#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA 'Programs\LCDSirPlus'),
    [switch]$PurgeUserData,
    [string]$ConfirmPurge,
    [string]$UserDataRoot = (Join-Path $env:LOCALAPPDATA 'LCDSirPlus'),
    [switch]$NoIntegration,
    [string]$ShortcutRoot = (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'),
    [string]$ShortcutName = 'LCDSirPlus.lnk',
    [string]$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run',
    [string]$RunName = 'LCDSirPlus',
    [switch]$SimulateRunning,
    [switch]$TestMode
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
. (Join-Path $PSScriptRoot 'Package.Common.ps1')

function Get-ShortcutTarget {
    param([string]$Path)
    if (-not [IO.File]::Exists($Path)) { return $null }
    Assert-RegularSingleLinkFile $Path | Out-Null
    $shell = New-Object -ComObject WScript.Shell
    return $shell.CreateShortcut($Path).TargetPath
}

function Get-RunValue {
    if (-not (Test-Path -LiteralPath $RunKey)) { return $null }
    $item = Get-ItemProperty -LiteralPath $RunKey -Name $RunName -ErrorAction SilentlyContinue
    if ($null -eq $item) { return $null }
    $property = $item.PSObject.Properties[$RunName]
    if ($null -eq $property) { return $null }
    return [string]$property.Value
}

function Invoke-UserDataPurge {
    param([string]$InstallPath)
    if (-not $PurgeUserData) { return }
    if ($ConfirmPurge -cne 'PURGE-LCDSIRPLUS-DATA') {
        throw 'purge requires -ConfirmPurge PURGE-LCDSIRPLUS-DATA'
    }
    $userData = Get-NormalizedFullPath $UserDataRoot
    $expected = Get-NormalizedFullPath (Join-Path $env:LOCALAPPDATA 'LCDSirPlus')
    if ($TestMode -and $env:LCDSIRPLUS_PACKAGE_TEST -cne '1') {
        throw 'test-mode purge requires the isolated package-test guard'
    }
    if (-not $TestMode -and -not $userData.Equals($expected, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'refusing to purge a noncanonical user-data root'
    }
    if ($userData.Equals($InstallPath, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'user-data root must not equal install root'
    }
    if ([IO.Directory]::Exists($userData)) {
        Assert-SafeLocalDirectory $userData | Out-Null
        Assert-SafeTree $userData
        Remove-Item -LiteralPath $userData -Recurse -Force
    }
}

$install = Get-NormalizedFullPath $InstallRoot
Assert-NotDangerousInstallRoot $install
Assert-SafeLeafName $ShortcutName
if (-not $RunKey.StartsWith('HKCU:\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Run key must remain beneath HKCU' }
if (-not [IO.Directory]::Exists($install)) {
    Invoke-UserDataPurge $install
    Write-Host 'LCDSirPlus is not installed at the selected root.'
    return
}
Assert-SafeLocalDirectory -Path $install | Out-Null
$executable = Join-Path $install 'LCDSirPlus.exe'
if (Test-ExactProcessRunning -ExecutablePath $executable -SimulateRunning:$SimulateRunning) {
    throw 'LCDSirPlus is running from the install root; uninstall refused'
}

$entries = @(Read-VerifiedManifest -Root $install -ManifestName 'INSTALL-MANIFEST.txt' -AllowedUndeclared @('lcdsirplus.txt') -AllowOtherFiles)

if (-not $NoIntegration) {
    $shortcutPath = Join-Path $ShortcutRoot $ShortcutName
    if ([IO.File]::Exists($shortcutPath)) {
        $target = Get-ShortcutTarget $shortcutPath
        if ($null -ne $target -and (Get-NormalizedFullPath $target).Equals((Get-NormalizedFullPath $executable), [StringComparison]::OrdinalIgnoreCase)) {
            Remove-Item -LiteralPath $shortcutPath -Force
        }
    }
    $runValue = Get-RunValue
    if ($null -ne $runValue -and (Test-CommandTargets -Command $runValue -ExecutablePath $executable)) {
        Remove-ItemProperty -LiteralPath $RunKey -Name $RunName -ErrorAction SilentlyContinue
    }
}

$owned = @($entries | Sort-Object { $_.Path.Length } -Descending)
$ownedDirectories = New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
foreach ($entry in $owned) {
    $parentPath = [IO.Path]::GetDirectoryName($entry.Path.Replace('/', '\'))
    while (-not [string]::IsNullOrEmpty($parentPath)) {
        [void]$ownedDirectories.Add($parentPath)
        $parentPath = [IO.Path]::GetDirectoryName($parentPath)
    }
    $path = Join-Path $install $entry.Path.Replace('/', '\')
    if ([IO.File]::Exists($path)) {
        $identity = Assert-RegularSingleLinkFile $path
        $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($identity.Size -ne $entry.Size -or $hash -ne $entry.Hash) { throw 'owned install file changed during uninstall' }
        Remove-Item -LiteralPath $path -Force
    }
}
$manifest = Join-Path $install 'INSTALL-MANIFEST.txt'
Assert-RegularSingleLinkFile $manifest | Out-Null
Remove-Item -LiteralPath $manifest -Force

$directories = @($ownedDirectories | Sort-Object { $_.Length } -Descending)
foreach ($relative in $directories) {
    $directory = Join-Path $install $relative
    if ([IO.Directory]::Exists($directory)) {
        $attributes = [IO.File]::GetAttributes($directory)
        if (($attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0 -and @(Get-ChildItem -LiteralPath $directory -Force).Count -eq 0) {
            [IO.Directory]::Delete($directory)
        }
    }
}
if (@(Get-ChildItem -LiteralPath $install -Force).Count -eq 0) {
    [IO.Directory]::Delete($install)
}

Invoke-UserDataPurge $install

Write-Host 'LCDSirPlus uninstall complete. Configuration and user data were preserved unless explicitly purged.' -ForegroundColor Green
