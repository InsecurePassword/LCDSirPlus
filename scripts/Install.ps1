#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$PackageRoot = $PSScriptRoot,
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA 'Programs\LCDForge2'),
    [switch]$EnableLogin,
    [switch]$NoIntegration,
    [string]$ShortcutRoot = (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'),
    [string]$ShortcutName = 'LCDForge 2.lnk',
    [string]$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run',
    [string]$RunName = 'LCDForge',
    [ValidateSet('None', 'AfterStage', 'AfterPublish', 'AfterIntegration')]
    [string]$InjectFailure = 'None',
    [switch]$SimulateRunning
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
. (Join-Path $PSScriptRoot 'Package.Common.ps1')

$payload = @(
    'lcdforge.exe',
    'lcdforge.txt',
    'LICENSE',
    'README.md',
    'RELEASE-NOTES.md',
    'SECURITY.md',
    'docs/ARCHITECTURE.md',
    'docs/CONFIGURATION.md',
    'docs/HARDWARE-ACCEPTANCE.md',
    'docs/PRODUCT-SPEC.md',
    'docs/REFERENCE-LAYOUT.md'
)

function Assert-ExpectedPackage {
    param([object[]]$Entries)
    $expected = @('Install.ps1', 'Package.Common.ps1', 'Uninstall.ps1')
    $expected += @($payload | ForEach-Object { 'payload/' + $_ })
    $expected = @(Get-OrdinalSorted $expected)
    $actual = @($Entries | ForEach-Object { $_.Path })
    if ($actual.Count -ne $expected.Count) { throw 'installer package inventory mismatch' }
    for ($index = 0; $index -lt $expected.Count; $index++) {
        if ($actual[$index] -cne $expected[$index]) { throw 'installer package inventory mismatch' }
    }
}

function Write-OwnershipManifest {
    param([string]$Root)
    $owned = @('Package.Common.ps1', 'Uninstall.ps1')
    $owned += @($payload | Where-Object { $_ -ne 'lcdforge.txt' })
    $owned = @(Get-OrdinalSorted $owned)
    $lines = @()
    foreach ($relative in $owned) {
        $path = Join-Path $Root $relative.Replace('/', '\')
        $identity = Assert-RegularSingleLinkFile $path
        $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        $lines += "{0}`t{1}`t{2}" -f $hash, $identity.Size, $relative
    }
    [IO.File]::WriteAllLines((Join-Path $Root 'INSTALL-MANIFEST.txt'), $lines, (New-Object Text.UTF8Encoding($false)))
}

function Get-ShortcutTarget {
    param([string]$Path)
    if (-not [IO.File]::Exists($Path)) { return $null }
    Assert-RegularSingleLinkFile $Path | Out-Null
    $shell = New-Object -ComObject WScript.Shell
    return $shell.CreateShortcut($Path).TargetPath
}

function Set-OwnedShortcut {
    param([string]$Path, [string]$Executable)
    $parent = Assert-SafeLocalDirectory -Path (Split-Path $Path -Parent) -Create
    $existing = Get-ShortcutTarget $Path
    if ($null -ne $existing -and -not (Get-NormalizedFullPath $existing).Equals((Get-NormalizedFullPath $Executable), [StringComparison]::OrdinalIgnoreCase)) {
        throw 'refusing to replace foreign Start Menu shortcut'
    }
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($Path)
    $shortcut.TargetPath = $Executable
    $shortcut.WorkingDirectory = Split-Path $Executable -Parent
    $shortcut.Description = 'LCDForge 2'
    $shortcut.Save()
    Assert-RegularSingleLinkFile $Path | Out-Null
}

function Get-RunValue {
    if (-not (Test-Path -LiteralPath $RunKey)) { return $null }
    $item = Get-ItemProperty -LiteralPath $RunKey -Name $RunName -ErrorAction SilentlyContinue
    if ($null -eq $item) { return $null }
    $property = $item.PSObject.Properties[$RunName]
    if ($null -eq $property) { return $null }
    return [string]$property.Value
}

$package = Assert-SafeLocalDirectory -Path $PackageRoot
$entries = @(Read-VerifiedManifest -Root $package)
Assert-ExpectedPackage $entries
Assert-SafeLeafName $ShortcutName
if (-not $RunKey.StartsWith('HKCU:\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Run key must remain beneath HKCU' }

$install = Get-NormalizedFullPath $InstallRoot
Assert-NotDangerousInstallRoot $install
$parent = Assert-SafeLocalDirectory -Path (Split-Path $install -Parent) -Create
if ([IO.Directory]::Exists($install)) { Assert-SafeLocalDirectory -Path $install | Out-Null }
$executable = Join-Path $install 'lcdforge.exe'
if (Test-ExactProcessRunning -ExecutablePath $executable -SimulateRunning:$SimulateRunning) {
    throw 'LCDForge is running from the install root; install/update refused'
}

$updating = [IO.Directory]::Exists($install) -and [IO.File]::Exists((Join-Path $install 'INSTALL-MANIFEST.txt'))
if ([IO.Directory]::Exists($install) -and -not $updating -and @(Get-ChildItem -LiteralPath $install -Force).Count -ne 0) {
    throw 'existing install root is not owned by LCDForge'
}
if ($updating) {
    [void]@(Read-VerifiedManifest -Root $install -ManifestName 'INSTALL-MANIFEST.txt' -AllowedUndeclared @('lcdforge.txt'))
}

$shortcutPath = Join-Path $ShortcutRoot $ShortcutName
$runBefore = $null
$shortcutBefore = $null
if (-not $NoIntegration) {
    if ([IO.File]::Exists($shortcutPath)) {
        $shortcutBefore = Get-ShortcutTarget $shortcutPath
        if (-not (Get-NormalizedFullPath $shortcutBefore).Equals((Get-NormalizedFullPath $executable), [StringComparison]::OrdinalIgnoreCase)) {
            throw 'foreign Start Menu shortcut uses the owned name'
        }
    }
    $runBefore = Get-RunValue
    if ($EnableLogin -and $null -ne $runBefore -and -not (Test-CommandTargets -Command $runBefore -ExecutablePath $executable)) {
        throw 'foreign HKCU Run value uses the owned name'
    }
}

$nonce = [Guid]::NewGuid().ToString('N')
$stage = Join-Path $parent ('.LCDForge2.stage.' + $nonce)
$backup = Join-Path $parent ('.LCDForge2.backup.' + $nonce)
$failed = Join-Path $parent ('.LCDForge2.failed.' + $nonce)
$published = $false
$backedUp = $false
$createdShortcut = $false
$createdRun = $false

try {
    [IO.Directory]::CreateDirectory($stage) | Out-Null
    Assert-SafeLocalDirectory -Path $stage | Out-Null
    foreach ($relative in $payload) {
        $source = Join-Path $package ('payload\' + $relative.Replace('/', '\'))
        $destination = Join-Path $stage $relative.Replace('/', '\')
        [IO.Directory]::CreateDirectory((Split-Path $destination -Parent)) | Out-Null
        Copy-Item -LiteralPath $source -Destination $destination
    }
    foreach ($script in @('Package.Common.ps1', 'Uninstall.ps1')) {
        Copy-Item -LiteralPath (Join-Path $package $script) -Destination (Join-Path $stage $script)
    }
    foreach ($entry in $entries) {
        if ($entry.Path -eq 'Install.ps1') { continue }
        $relative = if ($entry.Path.StartsWith('payload/')) { $entry.Path.Substring(8) } else { $entry.Path }
        $destination = Join-Path $stage $relative.Replace('/', '\')
        $identity = Assert-RegularSingleLinkFile $destination
        $hash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($identity.Size -ne $entry.Size -or $hash -ne $entry.Hash) { throw 'staged package member mismatch' }
    }
    if ($updating -and [IO.File]::Exists((Join-Path $install 'lcdforge.txt'))) {
        Assert-RegularSingleLinkFile (Join-Path $install 'lcdforge.txt') | Out-Null
        Copy-Item -LiteralPath (Join-Path $install 'lcdforge.txt') -Destination (Join-Path $stage 'lcdforge.txt') -Force
    }
    Write-OwnershipManifest $stage
    [void]@(Read-VerifiedManifest -Root $stage -ManifestName 'INSTALL-MANIFEST.txt' -AllowedUndeclared @('lcdforge.txt'))
    if ($InjectFailure -eq 'AfterStage') { throw 'injected failure after stage' }

    if ([IO.Directory]::Exists($install)) {
        if (@(Get-ChildItem -LiteralPath $install -Force).Count -eq 0) {
            [IO.Directory]::Delete($install)
        }
        else {
            [IO.Directory]::Move($install, $backup)
            $backedUp = $true
        }
    }
    [IO.Directory]::Move($stage, $install)
    $published = $true
    if ($InjectFailure -eq 'AfterPublish') { throw 'injected failure after publish' }
    [void]@(Read-VerifiedManifest -Root $install -ManifestName 'INSTALL-MANIFEST.txt' -AllowedUndeclared @('lcdforge.txt'))

    if (-not $NoIntegration) {
        $createdShortcut = $null -eq $shortcutBefore
        Set-OwnedShortcut -Path $shortcutPath -Executable $executable
        if ($EnableLogin) {
            if (-not (Test-Path -LiteralPath $RunKey)) { New-Item -Path $RunKey -Force | Out-Null }
            $createdRun = $null -eq $runBefore
            New-ItemProperty -LiteralPath $RunKey -Name $RunName -Value ('"{0}"' -f $executable) -PropertyType String -Force | Out-Null
        }
    }
    if ($InjectFailure -eq 'AfterIntegration') { throw 'injected failure after integration' }

    if ($backedUp) {
        Assert-SafeTree $backup
        Remove-Item -LiteralPath $backup -Recurse -Force
    }
    Write-Host ('LCDForge 0.3.0 installed at {0}' -f $install) -ForegroundColor Green
}
catch {
    $failure = $_
    if ($published -and [IO.Directory]::Exists($install)) {
        [IO.Directory]::Move($install, $failed)
    }
    if ($backedUp -and [IO.Directory]::Exists($backup)) {
        [IO.Directory]::Move($backup, $install)
    }
    foreach ($temporary in @($stage, $failed)) {
        if ([IO.Directory]::Exists($temporary)) {
            Assert-SafeTree $temporary
            Remove-Item -LiteralPath $temporary -Recurse -Force
        }
    }
    try {
        if ($createdRun -and (Test-Path -LiteralPath $RunKey)) {
            $value = Get-RunValue
            if (Test-CommandTargets -Command $value -ExecutablePath $executable) {
                Remove-ItemProperty -LiteralPath $RunKey -Name $RunName -ErrorAction SilentlyContinue
            }
        }
        if ($createdShortcut -and [IO.File]::Exists($shortcutPath)) {
            $target = Get-ShortcutTarget $shortcutPath
            if ((Get-NormalizedFullPath $target).Equals((Get-NormalizedFullPath $executable), [StringComparison]::OrdinalIgnoreCase)) {
                Remove-Item -LiteralPath $shortcutPath -Force
            }
        }
    }
    catch { }
    throw $failure
}
