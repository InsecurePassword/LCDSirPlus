#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$PackageRoot = $PSScriptRoot,
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA 'Programs\LCDSirPlus'),
    [switch]$EnableLogin,
    [switch]$NoIntegration,
    [string]$ShortcutRoot = (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'),
    [string]$ShortcutName = 'LCDSirPlus.lnk',
    [string]$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run',
    [string]$RunName = 'LCDSirPlus',
    [ValidateSet('None', 'AfterStage', 'AfterPublish', 'AfterIntegration')]
    [string]$InjectFailure = 'None',
    [switch]$SimulateRunning
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
. (Join-Path $PSScriptRoot 'Package.Common.ps1')

$payload = @(
    'LCDSirPlus.exe',
    'lcdsirplus.txt',
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
    $owned += @($payload | Where-Object { $_ -ne 'lcdsirplus.txt' })
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

function Get-ShortcutState {
    param([string]$Path)
    if (-not [IO.File]::Exists($Path)) { return $null }
    Assert-RegularSingleLinkFile $Path | Out-Null
    $file = Get-Item -LiteralPath $Path -Force
    return [pscustomobject]@{
        Bytes = [IO.File]::ReadAllBytes($Path)
        Attributes = $file.Attributes
        CreationTimeUtc = $file.CreationTimeUtc
        LastAccessTimeUtc = $file.LastAccessTimeUtc
        LastWriteTimeUtc = $file.LastWriteTimeUtc
        Target = Get-ShortcutTarget $Path
    }
}

function Restore-ShortcutState {
    param([string]$Path, [object]$Before, [string]$Executable)
    if ($null -eq $Before) {
        if ([IO.File]::Exists($Path) -and (Get-NormalizedFullPath (Get-ShortcutTarget $Path)).Equals((Get-NormalizedFullPath $Executable), [StringComparison]::OrdinalIgnoreCase)) {
            Remove-Item -LiteralPath $Path -Force
        }
        return
    }
    if ([IO.File]::Exists($Path)) { [IO.File]::SetAttributes($Path, [IO.FileAttributes]::Normal) }
    [IO.File]::WriteAllBytes($Path, $Before.Bytes)
    [IO.File]::SetCreationTimeUtc($Path, $Before.CreationTimeUtc)
    [IO.File]::SetLastWriteTimeUtc($Path, $Before.LastWriteTimeUtc)
    [IO.File]::SetLastAccessTimeUtc($Path, $Before.LastAccessTimeUtc)
    [IO.File]::SetAttributes($Path, $Before.Attributes)
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
    $shortcut.Description = 'LCDSirPlus Logitech LCD dashboard'
    $shortcut.Save()
    Assert-RegularSingleLinkFile $Path | Out-Null
}

function Get-RunState {
    if (-not (Test-Path -LiteralPath $RunKey)) { return $null }
    $key = Get-Item -LiteralPath $RunKey
    if (-not $key.GetValueNames().Contains($RunName)) { return $null }
    return [pscustomobject]@{
        Value = [string]$key.GetValue($RunName, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        Kind = $key.GetValueKind($RunName)
    }
}

function Restore-RunState {
    param([object]$Before, [string]$Executable)
    if ($null -ne $Before) {
        if (-not (Test-Path -LiteralPath $RunKey)) { New-Item -Path $RunKey -Force | Out-Null }
        New-ItemProperty -LiteralPath $RunKey -Name $RunName -Value $Before.Value -PropertyType $Before.Kind.ToString() -Force | Out-Null
        return
    }
    $current = Get-RunState
    if ($null -ne $current -and (Test-CommandTargets -Command $current.Value -ExecutablePath $Executable)) {
        Remove-ItemProperty -LiteralPath $RunKey -Name $RunName -ErrorAction SilentlyContinue
    }
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
$executable = Join-Path $install 'LCDSirPlus.exe'
if (Test-ExactProcessRunning -ExecutablePath $executable -SimulateRunning:$SimulateRunning) {
    throw 'LCDSirPlus is running from the install root; install/update refused'
}

$updating = [IO.Directory]::Exists($install) -and [IO.File]::Exists((Join-Path $install 'INSTALL-MANIFEST.txt'))
if ([IO.Directory]::Exists($install) -and -not $updating -and @(Get-ChildItem -LiteralPath $install -Force).Count -ne 0) {
    throw 'existing install root is not owned by LCDSirPlus'
}
if ($updating) {
    [void]@(Read-VerifiedManifest -Root $install -ManifestName 'INSTALL-MANIFEST.txt' -AllowedUndeclared @('lcdsirplus.txt'))
}

$shortcutPath = Join-Path $ShortcutRoot $ShortcutName
$runBefore = $null
$shortcutBefore = $null
if (-not $NoIntegration) {
    if ([IO.File]::Exists($shortcutPath)) {
        $shortcutBefore = Get-ShortcutState $shortcutPath
        if (-not (Get-NormalizedFullPath $shortcutBefore.Target).Equals((Get-NormalizedFullPath $executable), [StringComparison]::OrdinalIgnoreCase)) {
            throw 'foreign Start Menu shortcut uses the owned name'
        }
    }
    $runBefore = Get-RunState
    if ($EnableLogin -and $null -ne $runBefore -and -not (Test-CommandTargets -Command $runBefore.Value -ExecutablePath $executable)) {
        throw 'foreign HKCU Run value uses the owned name'
    }
}

$nonce = [Guid]::NewGuid().ToString('N')
$stage = Join-Path $parent ('.LCDSirPlus.stage.' + $nonce)
$backup = Join-Path $parent ('.LCDSirPlus.backup.' + $nonce)
$failed = Join-Path $parent ('.LCDSirPlus.failed.' + $nonce)
$published = $false
$backedUp = $false
$shortcutWriteStarted = $false
$runWriteStarted = $false

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
    if ($updating -and [IO.File]::Exists((Join-Path $install 'lcdsirplus.txt'))) {
        Assert-RegularSingleLinkFile (Join-Path $install 'lcdsirplus.txt') | Out-Null
        Copy-Item -LiteralPath (Join-Path $install 'lcdsirplus.txt') -Destination (Join-Path $stage 'lcdsirplus.txt') -Force
    }
    Write-OwnershipManifest $stage
    [void]@(Read-VerifiedManifest -Root $stage -ManifestName 'INSTALL-MANIFEST.txt' -AllowedUndeclared @('lcdsirplus.txt'))
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
    [void]@(Read-VerifiedManifest -Root $install -ManifestName 'INSTALL-MANIFEST.txt' -AllowedUndeclared @('lcdsirplus.txt'))

    if (-not $NoIntegration) {
        $shortcutWriteStarted = $true
        Set-OwnedShortcut -Path $shortcutPath -Executable $executable
        if ($EnableLogin) {
            if (-not (Test-Path -LiteralPath $RunKey)) { New-Item -Path $RunKey -Force | Out-Null }
            $runWriteStarted = $true
            New-ItemProperty -LiteralPath $RunKey -Name $RunName -Value ('"{0}"' -f $executable) -PropertyType String -Force | Out-Null
        }
    }
    if ($InjectFailure -eq 'AfterIntegration') { throw 'injected failure after integration' }

    if ($backedUp) {
        Assert-SafeTree $backup
        Remove-Item -LiteralPath $backup -Recurse -Force
    }
    Write-Host ('LCDSirPlus 0.3.0 installed at {0}' -f $install) -ForegroundColor Green
}
catch {
    $failure = $_
    $integrationRollbackFailure = $null
    try {
        if ($runWriteStarted) { Restore-RunState -Before $runBefore -Executable $executable }
        if ($shortcutWriteStarted) { Restore-ShortcutState -Path $shortcutPath -Before $shortcutBefore -Executable $executable }
    }
    catch { $integrationRollbackFailure = $_ }
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
    if ($null -ne $integrationRollbackFailure) { throw "installation failed and integration rollback failed: $integrationRollbackFailure" }
    throw $failure
}
