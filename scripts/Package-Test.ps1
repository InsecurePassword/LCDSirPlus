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
$portableName = "LCDForge-$version-win-x64-portable"
$installerName = "LCDForge-$version-win-x64-installer"
$sourceName = "LCDForge-$version-source"
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

Write-Host '== verify outer checksums and archive names ==' -ForegroundColor Cyan
$sumLines = @([IO.File]::ReadAllLines((Join-Path $artifactRoot 'SHA256SUMS.txt'), [Text.Encoding]::UTF8) | Where-Object { $_.Length -gt 0 })
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

$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('lcdforge-package-test-' + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($testRoot) | Out-Null
$runKey = 'HKCU:\Software\LCDForge2-PackageTest-' + [Guid]::NewGuid().ToString('N')
$env:LCDFORGE_PACKAGE_TEST = '1'

try {
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
    $exe = Join-Path $portable 'lcdforge.exe'
    & $exe --version
    if ($LASTEXITCODE -ne 0) { throw 'portable --version failed' }
    & $exe --validate-config --config (Join-Path $portable 'lcdforge.txt')
    if ($LASTEXITCODE -ne 0) { throw 'portable --validate-config failed' }
    $diagnostics = Join-Path $testRoot 'diagnostics'
    & $exe --config '\\unreachable.invalid\share\PACKAGE-PRIVATE-SENTINEL.txt' --diagnostics --diagnostic-dir $diagnostics
    if ($LASTEXITCODE -ne 0 -or @(Get-ChildItem -LiteralPath $diagnostics -Filter '*.zip').Count -ne 1) { throw 'portable diagnostics failed' }
    $diagnosticBundle = @(Get-ChildItem -LiteralPath $diagnostics -Filter '*.zip')[0].FullName
    $diagnosticText = [Text.Encoding]::Latin1.GetString([IO.File]::ReadAllBytes($diagnosticBundle))
    if ($diagnosticText -match 'PACKAGE-PRIVATE-SENTINEL|unreachable\.invalid') { throw 'diagnostics leaked ignored config path' }
    & $exe --hardware-test --backend virtual --duration-secs 1 --config (Join-Path $portable 'lcdforge.txt') --diagnostic-dir (Join-Path $testRoot 'logs')
    if ($LASTEXITCODE -ne 0) { throw 'portable virtual hardware smoke failed' }

    Write-Host '== installer lifecycle fixtures ==' -ForegroundColor Cyan
    $installRoot = Join-Path $testRoot 'install-main'
    & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $installRoot -NoIntegration
    $configPath = Join-Path $installRoot 'lcdforge.txt'
    $customConfig = [Text.Encoding]::UTF8.GetBytes("# PACKAGE-CONFIG-SENTINEL`r`n")
    [IO.File]::WriteAllBytes($configPath, $customConfig)
    & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $installRoot -NoIntegration
    if ([Convert]::ToBase64String($customConfig) -ne [Convert]::ToBase64String([IO.File]::ReadAllBytes($configPath))) { throw 'update changed configuration bytes' }

    $beforeExe = (Get-FileHash -LiteralPath (Join-Path $installRoot 'lcdforge.exe') -Algorithm SHA256).Hash
    Expect-Failure {
        & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $installRoot -NoIntegration -InjectFailure AfterPublish
    } 'rollback injection unexpectedly succeeded'
    if ((Get-FileHash -LiteralPath (Join-Path $installRoot 'lcdforge.exe') -Algorithm SHA256).Hash -ne $beforeExe) { throw 'rollback changed installed executable' }
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
    if ([IO.File]::Exists((Join-Path $installRoot 'lcdforge.exe'))) { throw 'uninstall retained owned executable' }

    $purgeInstall = Join-Path $testRoot 'install-purge'
    $userData = Join-Path $testRoot 'user-data'
    & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $purgeInstall -NoIntegration
    [IO.Directory]::CreateDirectory($userData) | Out-Null
    [IO.File]::WriteAllText((Join-Path $userData 'state.bin'), 'state')
    & (Join-Path $purgeInstall 'Uninstall.ps1') -InstallRoot $purgeInstall -NoIntegration -PurgeUserData -ConfirmPurge PURGE-LCDFORGE2-DATA -UserDataRoot $userData -TestMode
    if ([IO.Directory]::Exists($userData)) { throw 'confirmed user-data purge failed' }

    Write-Host '== shortcut and Run ownership fixtures ==' -ForegroundColor Cyan
    $integrationInstall = Join-Path $testRoot 'install-integration'
    $shortcutRoot = Join-Path $testRoot 'shortcuts'
    $shortcutName = 'LCDForge-PackageTest.lnk'
    $runName = 'LCDForge-PackageTest'
    & (Join-Path $installer 'Install.ps1') -PackageRoot $installer -InstallRoot $integrationInstall -EnableLogin -ShortcutRoot $shortcutRoot -ShortcutName $shortcutName -RunKey $runKey -RunName $runName
    if (-not [IO.File]::Exists((Join-Path $shortcutRoot $shortcutName))) { throw 'owned shortcut not created' }
    $runValue = (Get-ItemProperty -LiteralPath $runKey -Name $runName).$runName
    if (-not (Test-CommandTargets -Command $runValue -ExecutablePath (Join-Path $integrationInstall 'lcdforge.exe'))) { throw 'owned Run value not created' }
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
    $needles = @((Get-NormalizedFullPath $repo), (Get-NormalizedFullPath $testRoot))
    if (-not [string]::IsNullOrWhiteSpace($env:USERNAME)) { $needles += $env:USERNAME }
    foreach ($archive in $archives) {
        $text = [Text.Encoding]::Latin1.GetString([IO.File]::ReadAllBytes($archive))
        foreach ($needle in $needles) {
            if (-not [string]::IsNullOrEmpty($needle) -and $text.IndexOf($needle, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
                throw 'artifact contains a machine-specific or sentinel value'
            }
        }
    }
    foreach ($archive in @($portableZip, $installerZip)) {
        $text = [Text.Encoding]::Latin1.GetString([IO.File]::ReadAllBytes($archive))
        if ($text.IndexOf('PACKAGE-PRIVATE-SENTINEL', [StringComparison]::OrdinalIgnoreCase) -ge 0) {
            throw 'binary package contains the diagnostics sentinel'
        }
    }
}
finally {
    Remove-Item Env:\LCDFORGE_PACKAGE_TEST -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $runKey) { Remove-Item -LiteralPath $runKey -Recurse -Force }
    if ([IO.Directory]::Exists($testRoot)) { Remove-Item -LiteralPath $testRoot -Recurse -Force }
}

Write-Host ("PACKAGE TESTS PASSED: portable={0} installer={1} source={2} members" -f $portableCount, $installerCount, $sourceCount) -ForegroundColor Green
