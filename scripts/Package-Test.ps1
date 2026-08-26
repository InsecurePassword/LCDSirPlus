#Requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ArtifactDir,
    [Parameter(Mandatory = $true)][string]$ExpectedCommit
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
$repo = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'Package.Common.ps1')
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$version = '0.3.0'
$artifactRoot = Assert-SafeLocalDirectory -Path $ArtifactDir
$portableName = "LCDSirPlus-$version-win-x64-portable"
$installerName = "LCDSirPlus-$version-win-x64-installer"
$sourceName = "LCDSirPlus-$version-source"
$checksumName = "LCDSirPlus-$version-SHA256SUMS.txt"
$portableZip = Join-Path $artifactRoot ($portableName + '.zip')
$installerZip = Join-Path $artifactRoot ($installerName + '.zip')
$sourceZip = Join-Path $artifactRoot ($sourceName + '.zip')
$archives = @($installerZip, $portableZip, $sourceZip)

function Expect-Failure {
    param([scriptblock]$Action, [string]$Message)
    $failed = $false
    try { & $Action }
    catch { $failed = $true }
    if (-not $failed) { throw $Message }
}

function Test-ArchiveInventory {
    param([string]$Archive, [string]$Prefix)
    Assert-RegularSingleLinkFile $Archive | Out-Null
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    try {
        $names = @($zip.Entries | ForEach-Object { $_.FullName })
        Assert-ArchiveMemberNames $names
        $prefixText = $Prefix + '/'
        $inner = @()
        foreach ($name in $names) {
            if (-not $name.StartsWith($prefixText, [StringComparison]::Ordinal)) { throw 'archive member has wrong root' }
            $inner += $name.Substring($prefixText.Length)
        }
        Assert-ArchiveMemberNames $inner
        if (-not $inner.Contains('PACKAGE-MANIFEST.txt')) { throw 'archive manifest missing' }
        return $inner.Count
    }
    finally { $zip.Dispose() }
}

function Assert-PrivateBytesAbsent {
    param([byte[]]$Bytes, [string[]]$Needles, [switch]$SensitivePackage)
    $views = New-Object 'Collections.Generic.List[string]'
    [void]$views.Add([Text.Encoding]::Latin1.GetString($Bytes))
    foreach ($encoding in @([Text.Encoding]::Unicode, [Text.Encoding]::BigEndianUnicode)) {
        foreach ($offset in @(0, 1)) {
            $count = $Bytes.Length - $offset
            if (($count -band 1) -ne 0) { $count-- }
            if ($count -gt 0) { [void]$views.Add($encoding.GetString($Bytes, $offset, $count)) }
        }
    }
    foreach ($text in $views) {
        $retiredPattern = 'lcd' + '([-_ ]?)' + 'for' + 'ge2?'
        if ($text -match $retiredPattern) { throw 'package member contains the retired product identity' }
        foreach ($needle in $Needles) {
            if (-not [string]::IsNullOrWhiteSpace($needle) -and $text.IndexOf($needle, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
                throw 'package member contains a machine-specific value'
            }
        }
        if ($text -match '(?i)(?:[A-Z]:[\\/]|/)(?:Users|home)[\\/]') {
            throw 'package member contains a user-profile path'
        }
        if ($SensitivePackage -and $text -match '(?i)(PACKAGE-PRIVATE-SENTINEL|-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----|github_pat_[A-Za-z0-9_]{20,}|gh[pousr]_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{20,}|sk-[A-Za-z0-9]{20,})') {
            throw 'binary package contains a private-key or token sentinel'
        }
    }
}

function Assert-ArchivePrivacy {
    param([string]$Archive, [string[]]$Needles, [switch]$SensitivePackage)
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    try {
        foreach ($entry in $zip.Entries) {
            if ($entry.Length -gt 128MB) { throw 'archive member exceeds privacy scan bound' }
            $input = $entry.Open()
            $memory = New-Object IO.MemoryStream
            try {
                $input.CopyTo($memory)
                Assert-PrivateBytesAbsent -Bytes $memory.ToArray() -Needles $Needles -SensitivePackage:$SensitivePackage
            }
            finally { $memory.Dispose(); $input.Dispose() }
        }
    }
    finally { $zip.Dispose() }
}

Write-Host '== verify outer checksums and archive names ==' -ForegroundColor Cyan
$sumLines = @([IO.File]::ReadAllLines((Join-Path $artifactRoot $checksumName), [Text.Encoding]::UTF8) | Where-Object { $_.Length -gt 0 })
if ($sumLines.Count -ne 3) { throw 'outer checksum inventory mismatch' }
$lastName = $null
foreach ($line in $sumLines) {
    if ($line -notmatch '^([0-9a-f]{64})\t([0-9]+)\t([^\\/]+\.zip)$') { throw 'invalid outer checksum line' }
    $hash = $Matches[1]
    $size = [uint64]$Matches[2]
    $name = $Matches[3]
    if ($null -ne $lastName -and [StringComparer]::Ordinal.Compare($lastName, $name) -ge 0) { throw 'outer checksums not sorted' }
    $lastName = $name
    $path = Join-Path $artifactRoot $name
    if ((Assert-RegularSingleLinkFile $path).Size -ne $size) { throw 'outer size mismatch' }
    if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $hash) { throw 'outer hash mismatch' }
}

Expect-Failure { Assert-ArchiveMemberNames @('../escape') } 'traversal archive name accepted'
Expect-Failure { Assert-ArchiveMemberNames @('../escape/') } 'traversal archive directory accepted'
Expect-Failure { Assert-ArchiveMemberNames @('root/a', 'root/A') } 'case collision accepted'
Expect-Failure { Assert-ArchiveMemberNames @('root/a', 'root/a') } 'duplicate accepted'
Expect-Failure { Assert-ArchiveMemberNames @('root/file.txt:stream') } 'ADS name accepted'
Expect-Failure { Assert-ArchiveMemberNames @('root/CON.txt') } 'reserved name accepted'

$portableCount = Test-ArchiveInventory $portableZip $portableName
$installerCount = Test-ArchiveInventory $installerZip $installerName
$sourceCount = Test-ArchiveInventory $sourceZip $sourceName

$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('lcdsirplus-package-test-' + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($testRoot) | Out-Null
$runKey = 'HKCU:\Software\LCDSirPlus-PackageTest-' + [Guid]::NewGuid().ToString('N')
$env:LCDSIRPLUS_PACKAGE_TEST = '1'

try {
    $privacyFixture = Join-Path $testRoot 'privacy-fixture.zip'
    $privacyStream = [IO.File]::Open($privacyFixture, [IO.FileMode]::CreateNew)
    try {
        $privacyZip = New-Object IO.Compression.ZipArchive($privacyStream, [IO.Compression.ZipArchiveMode]::Create, $true)
        try {
            $privacyEntry = $privacyZip.CreateEntry('fixture/data.bin', [IO.Compression.CompressionLevel]::Optimal)
            $writer = New-Object IO.StreamWriter($privacyEntry.Open(), [Text.Encoding]::UTF8)
            try { $writer.Write('C:' + '\Users\LeakUser\private.txt') }
            finally { $writer.Dispose() }
        }
        finally { $privacyZip.Dispose() }
    }
    finally { $privacyStream.Dispose() }
    Expect-Failure { Assert-ArchivePrivacy -Archive $privacyFixture -Needles @() } 'compressed private path fixture was accepted'

    $portableExtract = Join-Path $testRoot 'portable'
    $installerExtract = Join-Path $testRoot 'installer'
    $sourceExtract = Join-Path $testRoot 'source'
    Expand-Archive -LiteralPath $portableZip -DestinationPath $portableExtract
    Expand-Archive -LiteralPath $installerZip -DestinationPath $installerExtract
    Expand-Archive -LiteralPath $sourceZip -DestinationPath $sourceExtract
    $portable = Join-Path $portableExtract $portableName
    $installer = Join-Path $installerExtract $installerName
    $source = Join-Path $sourceExtract $sourceName
    [void]@(Read-VerifiedManifest $portable)
    [void]@(Read-VerifiedManifest $installer)
    [void]@(Read-VerifiedManifest $source)

    Write-Host '== portable clean-extraction smoke ==' -ForegroundColor Cyan
    $exe = Join-Path $portable 'LCDSirPlus.exe'
    $versionOutput = (& $exe --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $versionOutput -cne 'LCDSirPlus 0.3.0') { throw 'portable --version identity failed' }
    $helpOutput = (& $exe --help | Out-String)
    if ($LASTEXITCODE -ne 0 -or $helpOutput -notmatch 'LCDSirPlus\.exe' -or $helpOutput -match ('lcd' + '([-_ ]?)' + 'for' + 'ge2?')) { throw 'portable --help identity failed' }
    & $exe --validate-config --config (Join-Path $portable 'lcdsirplus.txt')
    if ($LASTEXITCODE -ne 0) { throw 'portable --validate-config failed' }
    $diagnostics = Join-Path $testRoot 'diagnostics'
    & $exe --config '\\unreachable.invalid\share\PACKAGE-PRIVATE-SENTINEL.txt' --diagnostics --diagnostic-dir $diagnostics
    if ($LASTEXITCODE -ne 0 -or @(Get-ChildItem -LiteralPath $diagnostics -Filter '*.zip').Count -ne 1) { throw 'portable diagnostics failed' }
    $diagnosticBundle = @(Get-ChildItem -LiteralPath $diagnostics -Filter '*.zip')[0].FullName
    if ([IO.Path]::GetFileName($diagnosticBundle) -notmatch '^LCDSirPlus-Diagnostics-[0-9a-f]+\.zip$') { throw 'diagnostics filename identity mismatch' }
    $diagnosticText = [Text.Encoding]::Latin1.GetString([IO.File]::ReadAllBytes($diagnosticBundle))
    if ($diagnosticText -match 'PACKAGE-PRIVATE-SENTINEL|unreachable\.invalid') { throw 'diagnostics leaked ignored config path' }
    & $exe --hardware-test --backend virtual --duration-secs 1 --config (Join-Path $portable 'lcdsirplus.txt') --diagnostic-dir (Join-Path $testRoot 'logs')
    if ($LASTEXITCODE -ne 0) { throw 'portable virtual hardware smoke failed' }

    Write-Host '== installer lifecycle fixtures ==' -ForegroundColor Cyan
    $installRoot = Join-Path $testRoot 'install-main'
    & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $installRoot -NoIntegration
    $configPath = Join-Path $installRoot 'lcdsirplus.txt'
    $customConfig = [Text.Encoding]::UTF8.GetBytes("# PACKAGE-CONFIG-SENTINEL`r`n")
    [IO.File]::WriteAllBytes($configPath, $customConfig)
    & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $installRoot -NoIntegration
    if ([Convert]::ToBase64String($customConfig) -ne [Convert]::ToBase64String([IO.File]::ReadAllBytes($configPath))) { throw 'update changed configuration bytes' }

    $beforeExe = (Get-FileHash -LiteralPath (Join-Path $installRoot 'LCDSirPlus.exe') -Algorithm SHA256).Hash
    Expect-Failure {
        & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $installRoot -NoIntegration -InjectFailure AfterPublish
    } 'rollback injection unexpectedly succeeded'
    if ((Get-FileHash -LiteralPath (Join-Path $installRoot 'LCDSirPlus.exe') -Algorithm SHA256).Hash -ne $beforeExe) { throw 'rollback changed installed executable' }
    if ([Convert]::ToBase64String($customConfig) -ne [Convert]::ToBase64String([IO.File]::ReadAllBytes($configPath))) { throw 'rollback changed configuration' }
    Expect-Failure {
        & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $installRoot -NoIntegration -SimulateRunning
    } 'running-process refusal seam failed'

    $tampered = Join-Path $testRoot 'installer-tampered'
    Copy-Item -LiteralPath $installer -Destination $tampered -Recurse
    [IO.File]::AppendAllText((Join-Path $tampered 'payload\README.md'), 'tampered')
    Expect-Failure {
        & (Join-Path $tampered 'Install.ps1') -PackageRoot $tampered -InstallRoot (Join-Path $testRoot 'install-tampered') -NoIntegration
    } 'tampered package was accepted'

    [IO.File]::WriteAllText((Join-Path $installRoot 'unknown.keep'), 'preserve')
    & (Join-Path $installRoot 'Uninstall.ps1') -InstallRoot $installRoot -NoIntegration
    if (-not [IO.File]::Exists((Join-Path $installRoot 'unknown.keep')) -or -not [IO.File]::Exists($configPath)) { throw 'uninstall removed unknown file or configuration' }
    if ([IO.File]::Exists((Join-Path $installRoot 'LCDSirPlus.exe'))) { throw 'uninstall retained owned executable' }

    $purgeInstall = Join-Path $testRoot 'install-purge'
    $userData = Join-Path $testRoot 'user-data'
    & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $purgeInstall -NoIntegration
    [IO.Directory]::CreateDirectory($userData) | Out-Null
    [IO.File]::WriteAllText((Join-Path $userData 'state.bin'), 'state')
    & (Join-Path $purgeInstall 'Uninstall.ps1') -InstallRoot $purgeInstall -NoIntegration -PurgeUserData -ConfirmPurge PURGE-LCDSIRPLUS-DATA -UserDataRoot $userData -TestMode
    if ([IO.Directory]::Exists($userData)) { throw 'confirmed user-data purge failed' }

    Write-Host '== shortcut and Run ownership fixtures ==' -ForegroundColor Cyan
    $integrationInstall = Join-Path $testRoot 'install-integration'
    $shortcutRoot = Join-Path $testRoot 'shortcuts'
    $shortcutName = 'LCDSirPlus-PackageTest.lnk'
    $runName = 'LCDSirPlus-PackageTest'
    & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $integrationInstall -NoIntegration
    Expect-Failure {
        & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $integrationInstall -EnableLogin -ShortcutRoot $shortcutRoot -ShortcutName $shortcutName -RunKey $runKey -RunName $runName -InjectFailure AfterIntegration
    } 'new integration rollback injection unexpectedly succeeded'
    if ([IO.File]::Exists((Join-Path $shortcutRoot $shortcutName))) { throw 'failed install left a new shortcut' }
    $absentRun = Get-ItemProperty -LiteralPath $runKey -Name $runName -ErrorAction SilentlyContinue
    if ($null -ne $absentRun -and $null -ne $absentRun.PSObject.Properties[$runName]) { throw 'failed install left a new Run value' }

    & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $integrationInstall -EnableLogin -ShortcutRoot $shortcutRoot -ShortcutName $shortcutName -RunKey $runKey -RunName $runName
    $shortcutPath = Join-Path $shortcutRoot $shortcutName
    if (-not [IO.File]::Exists($shortcutPath)) { throw 'owned shortcut not created' }
    $runValue = (Get-ItemProperty -LiteralPath $runKey -Name $runName).$runName
    if (-not (Test-CommandTargets -Command $runValue -ExecutablePath (Join-Path $integrationInstall 'LCDSirPlus.exe'))) { throw 'owned Run value not created' }

    $ownedExe = Join-Path $integrationInstall 'LCDSirPlus.exe'
    $customRun = '"' + $ownedExe + '" --config "%TEMP%\lcdsirplus-owned.txt"'
    New-ItemProperty -LiteralPath $runKey -Name $runName -Value $customRun -PropertyType ExpandString -Force | Out-Null
    $customShortcut = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcutPath)
    $customShortcut.TargetPath = $ownedExe
    $customShortcut.Arguments = '--safe --config "owned custom.txt"'
    $customShortcut.IconLocation = $ownedExe + ',0'
    $customShortcut.WorkingDirectory = $integrationInstall
    $customShortcut.Description = 'Owned custom LCDSirPlus shortcut'
    $customShortcut.WindowStyle = 7
    $customShortcut.Save()
    $shortcutBeforeHash = (Get-FileHash -LiteralPath $shortcutPath -Algorithm SHA256).Hash
    $shortcutBeforeFile = Get-Item -LiteralPath $shortcutPath -Force
    $shortcutBeforeAttributes = $shortcutBeforeFile.Attributes
    $shortcutBeforeCreation = $shortcutBeforeFile.CreationTimeUtc
    $shortcutBeforeWrite = $shortcutBeforeFile.LastWriteTimeUtc
    Expect-Failure {
        & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $integrationInstall -EnableLogin -ShortcutRoot $shortcutRoot -ShortcutName $shortcutName -RunKey $runKey -RunName $runName -InjectFailure AfterIntegration
    } 'owned integration rollback injection unexpectedly succeeded'
    $restoredKey = Get-Item -LiteralPath $runKey
    $restoredRun = [string]$restoredKey.GetValue($runName, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    if ($restoredRun -cne $customRun -or $restoredKey.GetValueKind($runName) -ne [Microsoft.Win32.RegistryValueKind]::ExpandString) { throw 'owned Run value was not restored exactly' }
    if ((Get-FileHash -LiteralPath $shortcutPath -Algorithm SHA256).Hash -ne $shortcutBeforeHash) { throw 'owned shortcut bytes were not restored exactly' }
    $shortcutAfterFile = Get-Item -LiteralPath $shortcutPath -Force
    if ($shortcutAfterFile.Attributes -ne $shortcutBeforeAttributes -or $shortcutAfterFile.CreationTimeUtc -ne $shortcutBeforeCreation -or $shortcutAfterFile.LastWriteTimeUtc -ne $shortcutBeforeWrite) { throw 'owned shortcut metadata was not restored exactly' }
    $restoredShortcut = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcutPath)
    if ($restoredShortcut.TargetPath -cne $ownedExe -or $restoredShortcut.Arguments -cne '--safe --config "owned custom.txt"' -or $restoredShortcut.IconLocation -cne ($ownedExe + ',0') -or $restoredShortcut.WorkingDirectory -cne $integrationInstall -or $restoredShortcut.Description -cne 'Owned custom LCDSirPlus shortcut' -or $restoredShortcut.WindowStyle -ne 7) { throw 'owned shortcut behavior was not restored exactly' }

    & (Join-Path $integrationInstall 'Uninstall.ps1') -InstallRoot $integrationInstall -ShortcutRoot $shortcutRoot -ShortcutName $shortcutName -RunKey $runKey -RunName $runName
    if ([IO.File]::Exists((Join-Path $shortcutRoot $shortcutName))) { throw 'owned shortcut not removed' }
    $remainingRun = Get-ItemProperty -LiteralPath $runKey -Name $runName -ErrorAction SilentlyContinue
    if ($null -ne $remainingRun -and $null -ne $remainingRun.PSObject.Properties[$runName]) { throw 'owned Run value not removed' }

    $foreignInstall = Join-Path $testRoot 'install-foreign-integration'
    & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $foreignInstall -NoIntegration
    [IO.Directory]::CreateDirectory($shortcutRoot) | Out-Null
    $shell = New-Object -ComObject WScript.Shell
    $foreignShortcut = $shell.CreateShortcut((Join-Path $shortcutRoot $shortcutName))
    $foreignShortcut.TargetPath = "$env:SystemRoot\System32\notepad.exe"
    $foreignShortcut.Save()
    if (-not (Test-Path -LiteralPath $runKey)) { New-Item -Path $runKey -Force | Out-Null }
    New-ItemProperty -LiteralPath $runKey -Name $runName -Value '"C:\Windows\System32\notepad.exe"' -PropertyType String -Force | Out-Null
    Expect-Failure {
        & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $foreignInstall -EnableLogin -ShortcutRoot $shortcutRoot -ShortcutName $shortcutName -RunKey $runKey -RunName $runName
    } 'installer replaced foreign shortcut or Run ownership'
    & (Join-Path $foreignInstall 'Uninstall.ps1') -InstallRoot $foreignInstall -ShortcutRoot $shortcutRoot -ShortcutName $shortcutName -RunKey $runKey -RunName $runName
    if (-not [IO.File]::Exists((Join-Path $shortcutRoot $shortcutName))) { throw 'foreign shortcut was removed' }
    if ((Get-ItemProperty -LiteralPath $runKey -Name $runName).$runName -notmatch 'notepad') { throw 'foreign Run value was removed' }

    Write-Host '== source commit binding ==' -ForegroundColor Cyan
    if ([IO.File]::ReadAllText((Join-Path $source 'SOURCE-COMMIT.txt')).Trim() -ne $ExpectedCommit) { throw 'source commit marker mismatch' }
    $tracked = @(git -C $repo ls-tree -r --name-only $ExpectedCommit)
    if ($LASTEXITCODE -ne 0) { throw 'cannot enumerate committed source' }
    $tracked = @(Get-OrdinalSorted $tracked)
    $sourceFiles = @(Get-ChildItem -LiteralPath $source -File -Force -Recurse | ForEach-Object { Get-RelativePackagePath -Root $source -Path $_.FullName } | Where-Object { $_ -notin @('PACKAGE-MANIFEST.txt', 'SOURCE-COMMIT.txt') })
    $sourceFiles = @(Get-OrdinalSorted $sourceFiles)
    if ($tracked.Count -ne $sourceFiles.Count) { throw 'source archive member count mismatch' }
    for ($index = 0; $index -lt $tracked.Count; $index++) {
        if ($tracked[$index] -cne $sourceFiles[$index]) { throw 'source archive tracked inventory mismatch' }
        $blob = (git -C $repo rev-parse ($ExpectedCommit + ':' + $tracked[$index])).Trim()
        $actualBlob = (git -C $repo hash-object (Join-Path $source $tracked[$index].Replace('/', '\'))).Trim()
        if ($blob -ne $actualBlob) { throw 'source archive blob mismatch' }
    }

    Write-Host '== artifact privacy scan ==' -ForegroundColor Cyan
    $needles = @((Get-NormalizedFullPath $repo), (Get-NormalizedFullPath $testRoot), (Get-NormalizedFullPath ([IO.Path]::GetTempPath())))
    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) { $needles += Get-NormalizedFullPath $env:USERPROFILE }
    if (-not [string]::IsNullOrWhiteSpace($env:CARGO_HOME)) { $needles += Get-NormalizedFullPath $env:CARGO_HOME }
    if (-not [string]::IsNullOrWhiteSpace($env:USERNAME)) { $needles += $env:USERNAME }
    foreach ($archive in $archives) {
        Assert-ArchivePrivacy -Archive $archive -Needles $needles -SensitivePackage:($archive -ne $sourceZip)
    }
}
finally {
    Remove-Item Env:\LCDSIRPLUS_PACKAGE_TEST -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $runKey) { Remove-Item -LiteralPath $runKey -Recurse -Force }
    if ([IO.Directory]::Exists($testRoot)) { Remove-Item -LiteralPath $testRoot -Recurse -Force }
}

Write-Host ("PACKAGE TESTS PASSED: portable={0} installer={1} source={2} members" -f $portableCount, $installerCount, $sourceCount) -ForegroundColor Green
