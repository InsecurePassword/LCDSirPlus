#Requires -Version 7.0
[CmdletBinding()]
param(
    [string]$ArtifactDir,
    [ValidatePattern('^[0-9a-f]{40}$')][string]$ExpectedCommit,
    [string]$InstallerPayloadDir,
    [string]$SetupPath,
    [ValidatePattern('^[0-9a-f]{64}$')][string]$ExpectedSetupSha256,
    [ValidatePattern('^(?:[0-9a-f]{40}|WORKTREE-[0-9a-f]{64})$')][string]$ExpectedSourceIdentity,
    [switch]$ConfirmSystemMutation,
    [ValidateSet('All', 'InstallerStatic', 'CurrentUserLifecycle', 'AllUsersQualification', 'LifecycleCleanupStatic', 'ReleaseTransaction', 'NoticeInventory', 'SourcePrivacy')]
    [string]$Mode = 'All'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
$repo = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'Package.Common.ps1')
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

function Expect-Failure {
    param([scriptblock]$Action, [string]$Message)
    $failed = $false
    try { & $Action }
    catch { $failed = $true }
    if (-not $failed) { throw $Message }
}

function Assert-PresentMonPayload {
    param([string]$Root)
    $files = @(
        @('PresentMon.exe', [uint64]956768, '9bec3083069f58f911e6a512f4806db51a27bd096103087bc1d05ef54c80a191'),
        @('licenses/PresentMon/LICENSE.txt', [uint64]1067, '4c949341b1893c8c6ad82f7fb4eedf622cd1fd9c22a9af8f19b2dac19d1947b6'),
        @('licenses/PresentMon/THIRD_PARTY.txt', [uint64]6471, 'e039937f1a2fc2eb8f24a25b4229551a5a8056026d2da878507e20269eb56267')
    )
    foreach ($file in $files) {
        $path = Join-Path $Root $file[0].Replace('/', '\')
        $identity = Assert-RegularSingleLinkFile $path
        $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($identity.Size -ne $file[1] -or $hash -cne $file[2]) { throw "PresentMon payload identity mismatch: $($file[0])" }
    }
    $signature = Get-AuthenticodeSignature -FilePath (Join-Path $Root 'PresentMon.exe')
    $certificate = $signature.SignerCertificate
    $simpleName = if ($null -eq $certificate) { $null } else { $certificate.GetNameInfo([Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false) }
    if ($signature.Status -ne [Management.Automation.SignatureStatus]::Valid -or
        $simpleName -cne 'Intel Corporation') {
        throw "PresentMon Authenticode verification failed: $($signature.Status), $simpleName"
    }
}

function Assert-ExpectedInventory {
    param([object[]]$Entries, [string[]]$Expected, [string]$Label)
    $actual = @(Get-OrdinalSorted @($Entries | ForEach-Object { $_.Path }))
    $expectedSorted = @(Get-OrdinalSorted $Expected)
    if ($actual.Count -ne $expectedSorted.Count) { throw "$Label package inventory mismatch" }
    for ($index = 0; $index -lt $actual.Count; $index++) {
        if ($actual[$index] -cne $expectedSorted[$index]) { throw "$Label package inventory mismatch" }
    }
}

function Assert-SourceIdentityMarker {
    param([string]$Path, [string]$ExpectedIdentity, [switch]$RequireReadOnly)
    Assert-RegularSingleLinkFile $Path | Out-Null
    $bytes = [IO.File]::ReadAllBytes($Path)
    foreach ($byte in $bytes) {
        if ($byte -gt 0x7f) { throw 'source identity marker is not ASCII' }
    }
    $text = [Text.Encoding]::ASCII.GetString($bytes)
    if ($text -cnotmatch '^(?:[0-9a-f]{40}|WORKTREE-[0-9a-f]{64})\n\z') { throw 'source identity marker grammar mismatch' }
    $identity = $text.Substring(0, $text.Length - 1)
    if (-not [string]::IsNullOrWhiteSpace($ExpectedIdentity) -and $identity -cne $ExpectedIdentity) {
        throw 'source identity marker mismatch'
    }
    $expectedHash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData(
        [Text.Encoding]::ASCII.GetBytes($identity + "`n"))).ToLowerInvariant()
    if ((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $expectedHash) {
        throw 'source identity marker hash mismatch'
    }
    if ($RequireReadOnly -and -not ((Get-Item -LiteralPath $Path -Force).Attributes -band [IO.FileAttributes]::ReadOnly)) {
        throw 'installed source identity marker is not read-only'
    }
    return $identity
}

function Assert-ThirdPartyNoticeManifest {
    param([string]$Root, [object[]]$Entries, [string]$Relative = 'THIRD_PARTY_LICENSES.txt')
    $path = Join-Path $Root $Relative.Replace('/', '\')
    $identity = Assert-RegularSingleLinkFile $path
    $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    $entry = @($Entries | Where-Object { $_.Path -ceq $Relative })
    if ($entry.Count -ne 1 -or $entry[0].Size -ne $identity.Size -or $entry[0].Hash -cne $hash) {
        throw 'third-party notice is absent from or mismatched in package manifest'
    }
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
    param([byte[]]$Bytes, [string[]]$Needles, [switch]$SensitivePackage, [string]$RelativePath = '')
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
        $sensitivePattern = '(?i)(PACKAGE-PRIVATE-SENTINEL|-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----|github_pat_[A-Za-z0-9_]{20,}|gh[pousr]_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{20,}|sk-[A-Za-z0-9]{20,})'
        foreach ($match in [regex]::Matches($text, $sensitivePattern)) {
            $lineStart = $text.LastIndexOf("`n", $match.Index)
            $lineEnd = $text.IndexOf("`n", $match.Index)
            if ($lineStart -lt 0) { $lineStart = 0 } else { $lineStart++ }
            if ($lineEnd -lt 0) { $lineEnd = $text.Length }
            $line = $text.Substring($lineStart, $lineEnd - $lineStart)
            $scannerSentinel = 'PACKAGE-PRIVATE-' + 'SENTINEL'
            $pathParts = $RelativePath.Replace('\', '/').Split('/')
            $exactScannerPath = $pathParts.Count -eq 3 -and $pathParts[1] -ceq 'scripts' -and $pathParts[2] -ceq 'Package-Test.ps1'
            $inertScannerLiteral = $exactScannerPath -and
                $match.Value -ceq $scannerSentinel -and
                ($line.Contains('$sensitivePattern =', [StringComparison]::Ordinal) -or
                 $line.Contains('& $exe --config', [StringComparison]::Ordinal) -or
                 $line.Contains('$diagnosticText -match', [StringComparison]::Ordinal))
            if ($SensitivePackage -and -not $inertScannerLiteral) {
                throw "artifact member contains a private-key or token sentinel: $RelativePath"
            }
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
                Assert-PrivateBytesAbsent -Bytes $memory.ToArray() -Needles $Needles -SensitivePackage:$SensitivePackage -RelativePath $entry.FullName
            }
            finally { $memory.Dispose(); $input.Dispose() }
        }
    }
    finally { $zip.Dispose() }
}

function Test-InstallerStatic {
    param([string]$SetupExe, [string]$IssPath, [string]$PayloadRoot, [string]$ExpectedSetupHash, [string]$ExpectedIdentity)
    $identity = Assert-RegularSingleLinkFile $SetupExe
    if (-not [string]::IsNullOrWhiteSpace($ExpectedSetupHash) -and
        (Get-FileHash -LiteralPath $SetupExe -Algorithm SHA256).Hash.ToLowerInvariant() -cne $ExpectedSetupHash) {
        throw 'setup SHA-256 mismatch'
    }
    $stream = [IO.File]::OpenRead($SetupExe)
    try {
        $reader = New-Object IO.BinaryReader($stream)
        try {
            if ($reader.ReadUInt16() -ne 0x5a4d) { throw 'setup executable has no DOS signature' }
            $stream.Position = 0x3c
            $peOffset = $reader.ReadUInt32()
            $stream.Position = $peOffset
            if ($reader.ReadUInt32() -ne 0x00004550) { throw 'setup executable has no PE signature' }
        }
        finally { $reader.Dispose() }
    }
    finally { $stream.Dispose() }
    $versionInfo = (Get-Item -LiteralPath $SetupExe).VersionInfo
    if ($versionInfo.ProductName.Trim() -cne 'LCDSirPlus' -or $versionInfo.ProductVersion.Trim() -cne '0.3.0' -or
        $versionInfo.FileDescription.Trim() -cne 'LCDSirPlus Setup') {
        throw 'setup executable product metadata mismatch'
    }
    if ((Get-AuthenticodeSignature -FilePath $SetupExe).Status -ne [Management.Automation.SignatureStatus]::NotSigned) {
        throw 'setup executable unexpectedly has a signature'
    }
    $privateNeedles = @((Get-NormalizedFullPath $repo), (Get-NormalizedFullPath ([IO.Path]::GetTempPath())))
    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) { $privateNeedles += Get-NormalizedFullPath $env:USERPROFILE }
    if (-not [string]::IsNullOrWhiteSpace($env:USERNAME)) { $privateNeedles += $env:USERNAME }
    Assert-PrivateBytesAbsent -Bytes ([IO.File]::ReadAllBytes($SetupExe)) -Needles $privateNeedles -SensitivePackage

    $iss = [IO.File]::ReadAllText($IssPath, [Text.Encoding]::UTF8)
    $required = @(
        '#define AppId "{EEC8142F-9507-4D89-AF3F-D6BDD0804E5E}"',
        '#define UninstallKey "Software\Microsoft\Windows\CurrentVersion\Uninstall\{EEC8142F-9507-4D89-AF3F-D6BDD0804E5E}_is1"',
        '#define StartupTaskPrefix "LCDSirPlus_EEC8142F-9507-4D89-AF3F-D6BDD0804E5E_Startup_"',
        '#define TaskIdentity "LCDSirPlus {EEC8142F-9507-4D89-AF3F-D6BDD0804E5E}"',
        '#define TaskDescription "LCDSirPlus interactive startup task"',
        '#define OwnerMetadataName "LCDSirPlus-Owner.txt"',
        '#define OwnerMetadataHeader "LCDSirPlus installer owner v1"',
        '#ifndef SourceIdentity',
        '#error SourceIdentity must identify the explicitly staged source',
        '#define SourceMarkerHandle FileOpen(SourceMarkerPath)',
        '#if SourceMarkerIdentity != SourceIdentity',
        'AppId={{#AppId}',
        '#define AppVersion "0.3.0"',
        'AppVersion={#AppVersion}',
        'AppModifyPath={code:GetModifyPath}',
        'DefaultDirName={autopf}\LCDSirPlus',
        'PrivilegesRequired=lowest',
        'PrivilegesRequiredOverridesAllowed=dialog',
        'ArchitecturesAllowed=x64compatible',
        'ArchitecturesInstallIn64BitMode=x64compatible',
        'AppMutex=Local\LCDSirPlus.Runtime',
        'CloseApplications=yes',
        'RestartApplications=yes',
        'CompressionThreads=1',
        'LZMANumBlockThreads=1',
        'Encryption=no',
        'Name: "startmenu"; Description: "Create Start Menu shortcuts"',
        'Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked',
        'Name: "startup"; Description: "Start LCDSirPlus when I sign in"',
        'ExecAsOriginalUser',
        'Const TaskNotFound = -2147024894',
        'Function IsOwned(candidate)',
        'candidate.Actions.Count <> 1',
        'candidate.Triggers.Count <> 1',
        'candidate.Principal.LogonType <> 3 Or candidate.Principal.RunLevel <> 0',
        'ResolveAccountSid(triggerNodes.Item(0).Text)',
        'principalNodes.Item(0).Text',
        'candidate.XmlText',
        'candidateAction.Arguments',
        'candidateAction.WorkingDirectory',
        'candidate.RegistrationInfo.Source',
        'candidate.RegistrationInfo.Description',
        'PreviousPath',
        'function QuoteWindowsArg',
        'NUL, CR, and LF are not allowed in helper arguments.',
        'If WScript.Arguments.Count <> 10 Then WScript.Quit 26',
        'ExpectedPath = WScript.Arguments(1)',
        'ProductDescription = WScript.Arguments(8)',
        'TaskPrefix = WScript.Arguments(9)',
        'RegisterTaskDefinition TaskName, definition, 6',
        'Function RestorePrior()',
        'priorXml = existing.Xml',
        'function ReadOwnerMetadata',
        'function WriteOwnerMetadata',
        'RunTaskMutation(''validate-setup''',
        'function PrepareToInstall',
        'INSTALL-MANIFEST.txt',
        'Automatic migration is not supported.',
        'OriginalUserHasRegistration',
        'Registrations owned by other accounts may coexist.',
        'installer\LCDSirPlus-Setup.exe',
        'ModifyPath',
        'LCDSirPlus installer owner v1',
        'ScopeSwitch := ''/ALLUSERS''',
        'ScopeSwitch := ''/CURRENTUSER''',
        'function InitializeUninstall',
        'SuppressibleMsgBox(''LCDSirPlus uninstall stopped',
        'RunTaskMutation(''validate-uninstall''',
        'procedure CurUninstallStepChanged',
        'CurUninstallStep <> usUninstall',
        'RunTaskMutation(''uninstall-delete''',
        'uninstall stopped before removing application files',
        '[UninstallDelete]'
    )
    foreach ($text in $required) {
        if ($iss.IndexOf($text, [StringComparison]::Ordinal) -lt 0) { throw "installer source contract missing: $text" }
    }
    $modifyPathCode = [regex]::Match($iss,
        '(?s)function GetModifyPath\(const Param: String\): String;.*?(?=function OwnerMetadataPath)').Value
    foreach ($text in @(
        "ScopeSwitch := '/ALLUSERS'",
        "ScopeSwitch := '/CURRENTUSER'",
        "ExpandConstant('{app}\installer\LCDSirPlus-Setup.exe')",
        "Result := '`"' + ExpandConstant(")) {
        if ($modifyPathCode.IndexOf($text, [StringComparison]::Ordinal) -lt 0) { throw "native ModifyPath contract missing: $text" }
    }
    $ownerMetadataCode = [regex]::Match($iss,
        '(?s)function OwnerMetadataPath.*?(?=function TaskScript)').Value
    foreach ($text in @(
        "'installer\{#OwnerMetadataName}'",
        "Prefix := '{#OwnerMetadataHeader}' + #13#10",
        "CompareText(TaskName, '{#StartupTaskPrefix}' + OwnerSid)",
        'SaveStringToFile(OwnerMetadataPath',
        'ReadOwnerMetadata(ExpandConstant(')) {
        if ($ownerMetadataCode.IndexOf($text, [StringComparison]::Ordinal) -lt 0) { throw "owner metadata file contract missing: $text" }
    }
    foreach ($retired in @('LCDSirPlusInstallOwnerSid', 'LCDSirPlusTaskUserSid', 'LCDSirPlusTaskName', 'RegisterPreviousData', 'SetPreviousData')) {
        if ($iss.IndexOf($retired, [StringComparison]::Ordinal) -ge 0) { throw "retired custom uninstall metadata remains: $retired" }
    }
    $registrySection = [regex]::Match($iss, '(?ms)^\[Registry\]\s*$.*?(?=^\[[^]]+\]\s*$|\z)').Value
    if ($registrySection.IndexOf('{#UninstallKey}', [StringComparison]::Ordinal) -ge 0) {
        throw 'custom metadata must not use [Registry], which runs before Inno recreates its uninstall key'
    }
    $quoteHelper = [regex]::Match($iss,
        '(?s)function QuoteWindowsArg\(const Value: String\): String;.*?(?=function IsSidValue)').Value
    if ([string]::IsNullOrWhiteSpace($quoteHelper)) { throw 'Windows argv quote helper could not be isolated' }
    foreach ($text in @(
        '(Value[I] = #0) or (Value[I] = #13) or (Value[I] = #10)',
        'for J := 1 to (Backslashes * 2) + 1 do',
        'for J := 1 to Backslashes do',
        'for J := 1 to Backslashes * 2 do',
        'Result := Result + Value[I];')) {
        if ($quoteHelper.IndexOf($text, [StringComparison]::Ordinal) -lt 0) { throw "Windows argv quote contract missing: $text" }
    }
    if ($quoteHelper.IndexOf("Result := '`"';", [StringComparison]::Ordinal) -lt 0 -or
        $quoteHelper.IndexOf("if Value[I] = '\' then", [StringComparison]::Ordinal) -lt 0 -or
        $quoteHelper.IndexOf("if Value[I] = '`"' then", [StringComparison]::Ordinal) -lt 0) {
        throw 'Windows argv quote helper does not quote every argument or distinguish slash and quote runs'
    }

    $taskScriptSource = [regex]::Match($iss, '(?s)function TaskScript: String;.*?(?=function RunTaskMutation)').Value
    if ([string]::IsNullOrWhiteSpace($taskScriptSource)) { throw 'static task VBScript source could not be isolated' }
    $allScriptLines = @([regex]::Matches($taskScriptSource, '(?m)^\s*AddScriptLine\(Script,'))
    $literalScriptLines = @([regex]::Matches($taskScriptSource,
        "(?m)^\s*AddScriptLine\(Script, '((?:''|[^'])*)'\);\s*$"))
    if ($allScriptLines.Count -eq 0 -or $literalScriptLines.Count -ne $allScriptLines.Count) {
        throw 'task VBScript contains a non-static generated line'
    }
    $generatedTaskVbsLines = @($literalScriptLines | ForEach-Object { $_.Groups[1].Value.Replace("''", "'") })
    foreach ($line in $generatedTaskVbsLines) {
        foreach ($character in $line.ToCharArray()) {
            if ([int]$character -lt 0x20 -or [int]$character -gt 0x7e) { throw 'generated task VBScript is not printable ASCII' }
        }
    }
    $generatedTaskVbs = ($generatedTaskVbsLines -join "`r`n") + "`r`n"
    foreach ($dynamicLiteral in @(
        'LCDSirPlus interactive startup task',
        'LCDSirPlus {EEC8142F-9507-4D89-AF3F-D6BDD0804E5E}',
        'LCDSirPlus_EEC8142F-9507-4D89-AF3F-D6BDD0804E5E_Startup_')) {
        if ($generatedTaskVbs.IndexOf($dynamicLiteral, [StringComparison]::Ordinal) -ge 0) {
            throw 'generated task VBScript embeds a dynamic identity or description literal'
        }
    }
    if ($generatedTaskVbs -match '(?i)(?:[A-Z]:\\|\\\\)' -or
        $generatedTaskVbs -match 'S-1-(?:[0-9]+-)+[0-9]+' -or $generatedTaskVbs.Contains('<Task')) {
        throw 'generated task VBScript embeds an absolute path, concrete SID, or task XML literal'
    }
    if ($generatedTaskVbs.Contains('SameText(candidate.Principal.UserId, ExpectedSid)') -or
        $generatedTaskVbs.Contains('SameText(candidateTrigger.UserId, candidate.Principal.UserId)')) {
        throw 'task ownership verification compares scheduler-canonicalized account names directly to a SID'
    }
    if (-not [string]::IsNullOrWhiteSpace($PayloadRoot) -and
        $generatedTaskVbs.IndexOf((Get-NormalizedFullPath $PayloadRoot), [StringComparison]::OrdinalIgnoreCase) -ge 0) {
        throw 'generated task VBScript embeds the absolute staged payload path'
    }

    $runTaskMutation = [regex]::Match($iss, '(?s)function RunTaskMutation.*?(?=function OriginalUserHasRegistration)').Value
    if (@([regex]::Matches($runTaskMutation, 'QuoteWindowsArg\(')).Count -ne 11 -or
        @([regex]::Matches($runTaskMutation, '(?:ExecAsOriginalUser|Exec)\([^;]+?Parameters,')).Count -ne 2) {
        throw 'task helper does not quote the script and all ten dynamic arguments for both execution contexts'
    }
    $cleanupNew = [regex]::Match($generatedTaskVbs, '(?s)Function CleanupNew\(\).*?End Function').Value
    foreach ($text in @(
        'deleteError = Err.Number',
        'queryError = Err.Number',
        'CleanupNew = (deleteError = 0 And queryError = TaskNotFound)')) {
        if ($cleanupNew.IndexOf($text, [StringComparison]::Ordinal) -lt 0) { throw "CleanupNew fault seam missing: $text" }
    }
    foreach ($fault in @(
        @{ Delete = 0; Query = -2147024894; Expected = $true },
        @{ Delete = 5; Query = -2147024894; Expected = $false },
        @{ Delete = 0; Query = 0; Expected = $false },
        @{ Delete = 0; Query = -2147467259; Expected = $false })) {
        $actual = $fault.Delete -eq 0 -and $fault.Query -eq -2147024894
        if ($actual -ne $fault.Expected) { throw 'CleanupNew delete/query fault seam truth table failed' }
    }
    $initializeUninstall = [regex]::Match($iss,
        '(?s)function InitializeUninstall: Boolean;.*?(?=procedure CurUninstallStepChanged)',
        [Text.RegularExpressions.RegexOptions]::Multiline).Value
    if ($initializeUninstall -match "RunTaskMutation\('uninstall-delete'") {
        throw 'InitializeUninstall must validate only; task deletion belongs at usUninstall'
    }
    if ($initializeUninstall -match '(?m)^\s*MsgBox\(') {
        throw 'silent uninstall refusal contains a non-suppressible custom message box'
    }
    if ([string]::IsNullOrWhiteSpace($initializeUninstall)) { throw 'InitializeUninstall contract could not be isolated' }
    $postInstall = [regex]::Match($iss,
        '(?s)procedure CurStepChanged\(CurStep: TSetupStep\);.*?(?=function InitializeUninstall)').Value
    $prepareToInstall = [regex]::Match($iss,
        '(?s)function PrepareToInstall\(var NeedsRestart: Boolean\): String;.*?(?=procedure CurStepChanged)').Value
    $rollbackMetadata = [regex]::Match($iss,
        '(?s)function RollbackPreparedMetadata: Boolean;.*?(?=function PrepareToInstall)').Value
    $atomicCopy = [regex]::Match($iss,
        '(?s)function CopyFileAtomic\(const Source, Destination: String\): Boolean;.*?(?=function QuoteWindowsArg)').Value
    foreach ($contract in @(
        @($prepareToInstall, "SourceIsCache := CompareText(ExpandFileName(ExpandConstant('{srcexe}')),"),
        @($prepareToInstall, 'if HadPriorCache and not SourceIsCache and'),
        @($rollbackMetadata, 'if not SourceIsCache then begin'),
        @($postInstall, 'if not SourceIsCache and'),
        @($postInstall, "CopyFileAtomic(ExpandConstant('{srcexe}'), CachePath)"),
        @($atomicCopy, "PendingPath := Destination + '.new'"),
        @($atomicCopy, 'MoveFileReplaceExisting or MoveFileWriteThrough'))) {
        if ($contract[0].IndexOf($contract[1], [StringComparison]::Ordinal) -lt 0) {
            throw "cached setup self-path/atomic replacement contract missing: $($contract[1])"
        }
    }
    if ($postInstall.IndexOf("CopyFile(ExpandConstant('{srcexe}'), CachePath", [StringComparison]::Ordinal) -ge 0) {
        throw 'cached setup self-path can still copy the running setup onto itself'
    }
    $cachePreparation = $postInstall.IndexOf("CopyFileAtomic(ExpandConstant('{srcexe}')", [StringComparison]::Ordinal)
    $ownerMetadata = $postInstall.IndexOf('WriteOwnerMetadata(CurrentUserSid', [StringComparison]::Ordinal)
    $taskCommit = $postInstall.IndexOf("RunTaskMutation('create'", [StringComparison]::Ordinal)
    if ($cachePreparation -lt 0 -or $ownerMetadata -lt 0 -or $taskCommit -lt 0 -or
        $cachePreparation -gt $taskCommit -or $ownerMetadata -gt $taskCommit) {
        throw 'cache/owner metadata file must be verified before the startup task commit'
    }
    if ($postInstall.IndexOf('RegWriteStringValue', [StringComparison]::Ordinal) -ge 0 -or
        @([regex]::Matches($postInstall, 'RollbackPreparedMetadata')).Count -ne 2) {
        throw 'post-install must not write uninstall registration values and must roll back cache/owner files with task failures'
    }
    if ($iss -match '(?i)CurrentVersion\\Run|LibreHardwareMonitor|\bLHM\b|SignTool=|GetDateTimeString|GetEnv\(|\[UninstallRun\]|/REPAIR') {
        throw 'installer source contains a forbidden component, Run-key integration, script payload, or signing directive'
    }
    $buildSource = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'Build.ps1'), [Text.Encoding]::UTF8)
    if ($buildSource -match 'CurrentUserLifecycle|AllUsersQualification|ConfirmSystemMutation') {
        throw 'release Build.ps1 must not invoke installer lifecycle mutation modes'
    }
    if ($buildSource -match 'InnoSetup\\cache\\[^\r\n]*ISCC\.exe') {
        throw 'release Build.ps1 executes ISCC from the long-lived cache'
    }
    foreach ($text in @(
        "& (Join-Path `$PSScriptRoot 'Acquire-InnoSetup.ps1') -VerifyOnly",
        "`$innoRoot = Join-Path `$work ('inno-compiler-' + [Guid]::NewGuid().ToString('N'))",
        "& (Join-Path `$PSScriptRoot 'Acquire-InnoSetup.ps1') -ExtractVerified -OutputDir `$innoRoot",
        "`$iscc = Join-Path `$innoRoot 'ISCC.exe'",
        '("/DSourceIdentity=$head")',
        'Remove-Item -LiteralPath $work -Recurse -Force')) {
        if ($buildSource.IndexOf($text, [StringComparison]::Ordinal) -lt 0) { throw "fresh Inno compiler build contract missing: $text" }
    }
    $acquireSource = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'Acquire-InnoSetup.ps1'), [Text.Encoding]::UTF8)
    foreach ($text in @(
        "Assert-FileIdentity `$installer `$expected.Size `$expected.Hash 'Cached Inno Setup installer'",
        'Assert-InstallerSignature $installer',
        "'/PORTABLE=1'",
        'Start-Process -FilePath $installer',
        'Assert-PortableCompiler $output',
        'if (-not $succeeded -and [IO.Directory]::Exists($output))')) {
        if ($acquireSource.IndexOf($text, [StringComparison]::Ordinal) -lt 0) { throw "fresh Inno extraction contract missing: $text" }
    }
    $installerVerification = $acquireSource.IndexOf("Assert-FileIdentity `$installer `$expected.Size `$expected.Hash 'Cached Inno Setup installer'", [StringComparison]::Ordinal)
    $installerExecution = $acquireSource.IndexOf('Start-Process -FilePath $installer', [StringComparison]::Ordinal)
    $compilerVerification = $acquireSource.IndexOf('Assert-PortableCompiler $output', [StringComparison]::Ordinal)
    if ($installerVerification -lt 0 -or $installerExecution -le $installerVerification -or $compilerVerification -le $installerExecution) {
        throw 'Inno installer/compiler verification ordering contract changed'
    }
    $packageTestSource = [IO.File]::ReadAllText($PSCommandPath, [Text.Encoding]::UTF8)
    foreach ($text in @(
        '[char]::ConvertFromUtf32(0x1f680)',
        'current-user qualification path must not round-trip through the ANSI code page',
        'Assert-StartupTaskContract -InstallRoot $InstallRoot',
        'cached setup is not the exact qualification setup',
        'cached ModifyPath repair failed',
        'qualification uninstall failed')) {
        if ($packageTestSource.IndexOf($text, [StringComparison]::Ordinal) -lt 0) { throw "Unicode lifecycle source contract missing: $text" }
    }
    $sourceFiles = @([regex]::Matches($iss, '(?m)^Source: "\{#PayloadRoot\}\\([^\"]+)"; DestDir:') | ForEach-Object { $_.Groups[1].Value.Replace('\', '/') })
    $expectedFiles = @(
        'LCDSirPlus.exe', 'PresentMon.exe', 'lcdsirplus.default.txt', 'lcdsirplus.layout', 'SOURCE-COMMIT.txt',
        'README.md', 'modules.md', 'LICENSE', 'THIRD_PARTY_LICENSES.txt',
        'docs/CONFIGURATION.md', 'docs/INSTRUCTION-MANUAL.md',
        'docs/LCDSirPlus-Instruction-Manual.pdf', 'licenses/PresentMon/LICENSE.txt',
        'licenses/PresentMon/THIRD_PARTY.txt'
    )
    Assert-ExpectedInventory @($sourceFiles | ForEach-Object { [pscustomobject]@{ Path = $_ } }) $expectedFiles 'setup source'
    if (@([regex]::Matches($iss, '(?m)^Source: .*Flags: .*ignoreversion notimestamp\r?$')).Count -ne $expectedFiles.Count) {
        throw 'every setup payload file must disable source timestamps and restore on repair'
    }
    if ($iss.IndexOf('Source: "{#PayloadRoot}\SOURCE-COMMIT.txt"; DestDir: "{app}"; Attribs: readonly; Flags: overwritereadonly uninsremovereadonly ignoreversion notimestamp', [StringComparison]::Ordinal) -lt 0) {
        throw 'setup source identity marker read-only repair/uninstall contract is missing'
    }

    if (-not [string]::IsNullOrWhiteSpace($PayloadRoot)) {
        $payload = Assert-SafeLocalDirectory -Path $PayloadRoot
        $entries = @(Get-SafeTreeFiles $payload | ForEach-Object { [pscustomobject]@{ Path = Get-RelativePackagePath -Root $payload -Path $_.FullName } })
        Assert-ExpectedInventory $entries $expectedFiles 'staged setup'
        if ([Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes((Join-Path $payload 'lcdsirplus.layout'))) -cne 'installed-v1') {
            throw 'installed layout marker is not exact'
        }
        [void](Assert-SourceIdentityMarker -Path (Join-Path $payload 'SOURCE-COMMIT.txt') -ExpectedIdentity $ExpectedIdentity)
        Assert-PresentMonPayload $payload
    }
    return $identity.Size
}

$innoUninstallSubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{EEC8142F-9507-4D89-AF3F-D6BDD0804E5E}_is1'
$startupTaskPrefix = 'LCDSirPlus_EEC8142F-9507-4D89-AF3F-D6BDD0804E5E_Startup_'
$currentUserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$startupTaskName = $startupTaskPrefix + $currentUserSid
$taskIdentity = 'LCDSirPlus {EEC8142F-9507-4D89-AF3F-D6BDD0804E5E}'
$taskDescription = 'LCDSirPlus interactive startup task'

function Get-UninstallRegistration {
    param([switch]$Machine)
    $hive = if ($Machine) { [Microsoft.Win32.RegistryHive]::LocalMachine } else { [Microsoft.Win32.RegistryHive]::CurrentUser }
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey($hive, [Microsoft.Win32.RegistryView]::Registry64)
    try {
        $key = $base.OpenSubKey($innoUninstallSubkey, $false)
        if ($null -eq $key) { return $null }
        try {
            $values = @{}
            foreach ($name in @('DisplayName', 'InstallLocation', 'ModifyPath', 'UninstallString')) {
                $values[$name] = [string]$key.GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            }
            $values.CustomOwnerValuesPresent = @($key.GetValueNames() | Where-Object {
                $_ -in @('LCDSirPlusInstallOwnerSid', 'LCDSirPlusTaskUserSid', 'LCDSirPlusTaskName')
            }).Count -ne 0
            return [pscustomobject]$values
        }
        finally { $key.Dispose() }
    }
    finally { $base.Dispose() }
}

function Invoke-QualificationProcess {
    param([string]$FilePath, [string[]]$Arguments, [string]$Label = 'qualification process')
    $process = Start-Process -FilePath $FilePath -ArgumentList $Arguments -Wait -PassThru
    try {
        Write-Host ("{0}: exit {1}" -f $Label, $process.ExitCode)
        return $process.ExitCode
    }
    finally { $process.Dispose() }
}

function Invoke-BestEffortCleanup {
    param([string]$Label, [scriptblock]$Action)
    try { & $Action }
    catch { Write-Warning "$Label cleanup failed: $_" }
}

function Get-IdentitySid {
    param([string]$Identity)
    if ($Identity -match '^S-1-[0-9-]+$') { return $Identity }
    return ([Security.Principal.NTAccount]::new($Identity)).Translate([Security.Principal.SecurityIdentifier]).Value
}

function Get-ScheduledTaskXmlText {
    param([string]$Name, [switch]$AllowMissing)
    $service = New-Object -ComObject 'Schedule.Service'
    $service.Connect()
    $root = $service.GetFolder('\')
    try { $task = $root.GetTask($Name) }
    catch {
        if ($AllowMissing -and $_.Exception.HResult -eq -2147024894) { return $null }
        throw "scheduled task query failed for ${Name}: $_"
    }
    if ($null -eq $task) {
        if ($AllowMissing) { return $null }
        throw "scheduled task query returned no task without a not-found error: $Name"
    }
    return [string]$task.Xml
}

function Get-StartupTaskXml {
    param([switch]$AllowMissing)
    $text = Get-ScheduledTaskXmlText -Name $startupTaskName -AllowMissing:$AllowMissing
    if ($null -eq $text) { return $null }
    return [xml]$text
}

function Get-UserDataMetadata {
    param([string]$Root)
    if (-not [IO.Directory]::Exists($Root)) { return @() }
    $rootFull = Assert-SafeLocalDirectory -Path $Root
    $entries = @()
    foreach ($item in Get-ChildItem -LiteralPath $rootFull -Force -Recurse) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "canonical user data contains an untrusted reparse point: $($item.FullName)"
        }
        $relative = [IO.Path]::GetRelativePath($rootFull, $item.FullName).Replace('\', '/')
        if ($item.PSIsContainer) {
            $entries += "D`t$relative`t$([int]$item.Attributes)`t$($item.CreationTimeUtc.Ticks)`t$($item.LastWriteTimeUtc.Ticks)"
        }
        else {
            $identity = Assert-RegularSingleLinkFile $item.FullName
            $entries += "F`t$relative`t$([int]$item.Attributes)`t$($item.CreationTimeUtc.Ticks)`t$($item.LastWriteTimeUtc.Ticks)`t$($identity.Size)`t$($identity.Volume)`t$($identity.Index)"
        }
    }
    return @(Get-OrdinalSorted $entries)
}

function Assert-MetadataEqual {
    param([string[]]$Expected, [string[]]$Actual, [string]$Label)
    if ($Expected.Count -ne $Actual.Count) { throw "$Label metadata inventory changed" }
    for ($index = 0; $index -lt $Expected.Count; $index++) {
        if ($Expected[$index] -cne $Actual[$index]) { throw "$Label metadata changed" }
    }
}

function Test-RegistrationTargetsRoot {
    param([object]$Registration, [string]$Root)
    if ($null -eq $Registration -or [string]::IsNullOrWhiteSpace($Root) -or
        $Registration.DisplayName -cne 'LCDSirPlus 0.3.0' -or
        $Registration.CustomOwnerValuesPresent) { return $false }
    try {
        if (-not (Get-NormalizedFullPath $Registration.InstallLocation).Equals((Get-NormalizedFullPath $Root), [StringComparison]::OrdinalIgnoreCase)) { return $false }
        if ($Registration.UninstallString -cnotmatch '^"([^\"]+\.exe)"$') { return $false }
        if (-not (Get-NormalizedFullPath $Matches[1]).StartsWith((Get-NormalizedFullPath $Root) + '\', [StringComparison]::OrdinalIgnoreCase)) { return $false }
        $owner = Get-InstallerOwnerMetadata -Root $Root
        return $owner.OwnerSid -ceq $currentUserSid
    }
    catch { return $false }
}

function Remove-ExactUninstallRegistration {
    param([switch]$Machine)
    $hive = if ($Machine) { [Microsoft.Win32.RegistryHive]::LocalMachine } else { [Microsoft.Win32.RegistryHive]::CurrentUser }
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey($hive, [Microsoft.Win32.RegistryView]::Registry64)
    try { $base.DeleteSubKeyTree($innoUninstallSubkey, $false) }
    finally { $base.Dispose() }
}

function Get-OnlyXmlNode {
    param([xml]$Xml, [string]$XPath, [string]$Label)
    $nodes = @($Xml.SelectNodes($XPath))
    if ($nodes.Count -ne 1) { throw "$Label XML inventory mismatch" }
    return $nodes[0]
}

function Get-InstallerOwnerMetadata {
    param([string]$Root)
    $path = Join-Path $Root 'installer\LCDSirPlus-Owner.txt'
    Assert-RegularSingleLinkFile $path | Out-Null
    $text = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($path))
    $prefix = "LCDSirPlus installer owner v1`r`n"
    if (-not $text.StartsWith($prefix, [StringComparison]::Ordinal)) { throw 'installer owner metadata header mismatch' }
    $lines = $text.Substring($prefix.Length).Split("`r`n", [StringSplitOptions]::None)
    if ($lines.Count -ne 3 -or $lines[0] -cnotmatch '^S-1-[0-9]+(?:-[0-9]+)+$' -or
        ($lines[1] -cne '' -and $lines[1] -cne ($startupTaskPrefix + $lines[0])) -or $lines[2] -cne '') {
        throw 'installer owner metadata contract mismatch'
    }
    return [pscustomobject]@{ OwnerSid = $lines[0]; TaskName = $lines[1] }
}

function Assert-StartupTaskContract {
    param([string]$InstallRoot, [string]$ExpectedSid)
    $xml = Get-StartupTaskXml
    $prefix = "/*[local-name()='Task']"
    $source = Get-OnlyXmlNode $xml ($prefix + "/*[local-name()='RegistrationInfo']/*[local-name()='Source']") 'task source'
    $description = Get-OnlyXmlNode $xml ($prefix + "/*[local-name()='RegistrationInfo']/*[local-name()='Description']") 'task description'
    if (@($xml.SelectNodes($prefix + "/*[local-name()='Actions']/*")).Count -ne 1 -or
        @($xml.SelectNodes($prefix + "/*[local-name()='Triggers']/*")).Count -ne 1) {
        throw 'task action or trigger XML inventory mismatch'
    }
    $action = Get-OnlyXmlNode $xml ($prefix + "/*[local-name()='Actions']/*[local-name()='Exec']") 'task action'
    $command = Get-OnlyXmlNode $xml ($prefix + "/*[local-name()='Actions']/*[local-name()='Exec']/*[local-name()='Command']") 'task command'
    $working = Get-OnlyXmlNode $xml ($prefix + "/*[local-name()='Actions']/*[local-name()='Exec']/*[local-name()='WorkingDirectory']") 'task working directory'
    $arguments = @($xml.SelectNodes($prefix + "/*[local-name()='Actions']/*[local-name()='Exec']/*[local-name()='Arguments']"))
    $trigger = Get-OnlyXmlNode $xml ($prefix + "/*[local-name()='Triggers']/*[local-name()='LogonTrigger']") 'task trigger'
    $triggerUser = Get-OnlyXmlNode $xml ($prefix + "/*[local-name()='Triggers']/*[local-name()='LogonTrigger']/*[local-name()='UserId']") 'task trigger user'
    $principal = Get-OnlyXmlNode $xml ($prefix + "/*[local-name()='Principals']/*[local-name()='Principal']") 'task principal'
    $principalUser = Get-OnlyXmlNode $xml ($prefix + "/*[local-name()='Principals']/*[local-name()='Principal']/*[local-name()='UserId']") 'task principal user'
    $logonType = Get-OnlyXmlNode $xml ($prefix + "/*[local-name()='Principals']/*[local-name()='Principal']/*[local-name()='LogonType']") 'task logon type'
    $runLevels = @($xml.SelectNodes($prefix + "/*[local-name()='Principals']/*[local-name()='Principal']/*[local-name()='RunLevel']"))
    if ($runLevels.Count -gt 1) { throw 'task run level XML inventory mismatch' }
    if ($source.InnerText -cne $taskIdentity -or $description.InnerText -cne $taskDescription -or
        $command.InnerText -cne (Join-Path $InstallRoot 'LCDSirPlus.exe') -or
        $working.InnerText -cne $InstallRoot -or $arguments.Count -gt 1 -or
        ($arguments.Count -eq 1 -and $arguments[0].InnerText -cne '') -or
        $logonType.InnerText -cne 'InteractiveToken' -or
        ($runLevels.Count -eq 1 -and $runLevels[0].InnerText -cne 'LeastPrivilege') -or
        (Get-IdentitySid $principalUser.InnerText) -cne $ExpectedSid -or
        (Get-IdentitySid $triggerUser.InnerText) -cne $ExpectedSid) {
        throw 'startup task XML ownership contract mismatch'
    }
    [void]$action
    [void]$trigger
    [void]$principal
}

function Assert-QualificationInstall {
    param([string]$InstallRoot, [string]$Setup, [object]$Registration, [string]$ScopeSwitch, [string]$ExpectedIdentity, [switch]$Machine)
    $cache = Join-Path $InstallRoot 'installer\LCDSirPlus-Setup.exe'
    $expectedModify = '"' + $cache + '" ' + $ScopeSwitch
    $owner = Get-InstallerOwnerMetadata -Root $InstallRoot
    if ($null -eq $Registration -or $Registration.DisplayName -cne 'LCDSirPlus 0.3.0' -or
        -not (Get-NormalizedFullPath $Registration.InstallLocation).Equals((Get-NormalizedFullPath $InstallRoot), [StringComparison]::OrdinalIgnoreCase) -or
        $Registration.ModifyPath -cne $expectedModify -or
        $Registration.CustomOwnerValuesPresent -or $owner.OwnerSid -cne $currentUserSid -or
        $owner.TaskName -cne $startupTaskName) {
        throw 'Apps & Features registration or owner metadata file mismatch'
    }
    foreach ($relative in @('LCDSirPlus.exe', 'PresentMon.exe', 'lcdsirplus.default.txt', 'lcdsirplus.layout',
        'README.md', 'modules.md', 'LICENSE', 'THIRD_PARTY_LICENSES.txt', 'SOURCE-COMMIT.txt', 'docs\CONFIGURATION.md',
        'docs\INSTRUCTION-MANUAL.md', 'docs\LCDSirPlus-Instruction-Manual.pdf',
        'licenses\PresentMon\LICENSE.txt', 'licenses\PresentMon\THIRD_PARTY.txt',
        'installer\LCDSirPlus-Setup.exe', 'installer\LCDSirPlus-Owner.txt')) {
        Assert-RegularSingleLinkFile (Join-Path $InstallRoot $relative) | Out-Null
    }
    foreach ($forbidden in @('Install.ps1', 'Uninstall.ps1', 'Package.Common.ps1')) {
        if ([IO.File]::Exists((Join-Path $InstallRoot $forbidden))) { throw "installed payload contains legacy script $forbidden" }
    }
    if ([Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes((Join-Path $InstallRoot 'lcdsirplus.layout'))) -cne 'installed-v1') {
        throw 'installed layout marker mismatch'
    }
    [void](Assert-SourceIdentityMarker -Path (Join-Path $InstallRoot 'SOURCE-COMMIT.txt') -ExpectedIdentity $ExpectedIdentity -RequireReadOnly)
    if ((Get-FileHash -LiteralPath $cache -Algorithm SHA256).Hash -cne (Get-FileHash -LiteralPath $Setup -Algorithm SHA256).Hash) {
        throw 'cached setup is not the exact qualification setup'
    }
    Assert-StartupTaskContract -InstallRoot $InstallRoot -ExpectedSid $owner.OwnerSid
    $programs = if ($Machine) { Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\LCDSirPlus' } else { Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\LCDSirPlus' }
    foreach ($shortcut in @('LCDSirPlus.lnk', 'Uninstall LCDSirPlus.lnk')) {
        Assert-RegularSingleLinkFile (Join-Path $programs $shortcut) | Out-Null
    }
    $desktop = if ($Machine) { Join-Path $env:PUBLIC 'Desktop\LCDSirPlus.lnk' } else { Join-Path ([Environment]::GetFolderPath('Desktop')) 'LCDSirPlus.lnk' }
    if ([IO.File]::Exists($desktop)) { throw 'desktop shortcut was created although its task is unchecked' }
    if ($Registration.UninstallString -cnotmatch '^"([^"]+\.exe)"$' -or -not [IO.File]::Exists($Matches[1])) {
        throw 'standard uninstaller registration is missing or unsafe'
    }
}

function Add-QualificationLog {
    param([string[]]$Arguments, [string]$LogRoot, [string]$Phase)
    return @($Arguments) + ('/LOG="' + (Join-Path $LogRoot ($Phase + '.log')) + '"')
}

function Assert-QualificationSentinel {
    param([string]$Path, [byte[]]$ExpectedBytes, [object]$ExpectedIdentity)
    $identity = Assert-RegularSingleLinkFile $Path
    if ($identity.Volume -ne $ExpectedIdentity.Volume -or $identity.Index -ne $ExpectedIdentity.Index -or
        $identity.Size -ne $ExpectedIdentity.Size -or
        [Convert]::ToBase64String([IO.File]::ReadAllBytes($Path)) -cne [Convert]::ToBase64String($ExpectedBytes)) {
        throw 'qualification sentinel identity or bytes changed'
    }
}

function Remove-QualificationShortcut {
    param([string]$Path, [string]$InstallRoot, [switch]$Uninstaller)
    if (-not [IO.File]::Exists($Path)) { return }
    Assert-RegularSingleLinkFile $Path | Out-Null
    $shortcut = (New-Object -ComObject WScript.Shell).CreateShortcut($Path)
    $target = Get-NormalizedFullPath $shortcut.TargetPath
    $root = Get-NormalizedFullPath $InstallRoot
    $expected = if ($Uninstaller) {
        if ([IO.Path]::GetFileName($target) -cnotmatch '^unins[0-9]*\.exe$') { throw "uninstall shortcut ownership is uncertain: $Path" }
        Join-Path $root ([IO.Path]::GetFileName($target))
    }
    else { Join-Path $root 'LCDSirPlus.exe' }
    $ansi = [Text.Encoding]::GetEncoding([Globalization.CultureInfo]::CurrentCulture.TextInfo.ANSICodePage)
    $ansiExpected = $ansi.GetString($ansi.GetBytes($expected))
    if (-not $target.Equals($expected, [StringComparison]::OrdinalIgnoreCase) -and
        -not $target.Equals($ansiExpected, [StringComparison]::OrdinalIgnoreCase)) {
        throw "application shortcut ownership is uncertain: $Path"
    }
    [IO.File]::Delete($Path)
}

function Invoke-InstallerLifecycleQualification {
    param([string]$Setup, [string]$ExpectedSetupHash, [string]$ExpectedIdentity, [switch]$Machine)
    $isElevated = ([Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if ($Machine -and -not $isElevated) { throw 'AllUsersQualification requires an elevated PowerShell session' }
    if (-not $Machine -and $isElevated) { throw 'CurrentUserLifecycle must run from a non-elevated PowerShell session' }
    if ($null -ne (Get-UninstallRegistration) -or $null -ne (Get-UninstallRegistration -Machine)) {
        throw 'qualification requires no existing LCDSirPlus registration in either scope'
    }
    if ($null -ne (Get-StartupTaskXml -AllowMissing)) { throw 'qualification requires the LCDSirPlus startup task name to be unused' }
    $legacyTaskXml = Get-ScheduledTaskXmlText -Name '\LCDSirPlus' -AllowMissing
    $legacyTaskHash = if ($null -eq $legacyTaskXml) { $null } else {
        [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($legacyTaskXml))).ToLowerInvariant()
    }
    Write-Host "Legacy task before: present=$($null -ne $legacyTaskXml) sha256=$legacyTaskHash"

    $id = [Guid]::NewGuid().ToString('N')
    $userData = Join-Path $env:LOCALAPPDATA 'LCDSirPlus'
    if ([IO.File]::Exists($userData)) { throw 'canonical LCDSirPlus user data is a file, not a directory' }
    $userDataExisted = [IO.Directory]::Exists($userData)
    $userDataBefore = @(Get-UserDataMetadata $userData)
    $sentinelDir = Join-Path $userData ('.qualification-' + $id)
    $sentinelPath = Join-Path $sentinelDir 'sentinel.bin'
    if ([IO.Directory]::Exists($sentinelDir) -or [IO.File]::Exists($sentinelDir)) { throw 'GUID qualification sentinel path unexpectedly exists' }
    Write-Host "Qualification parents before: LocalAppData=$([IO.Directory]::Exists($env:LOCALAPPDATA)) userData=$userDataExisted sentinelDir=False"

    $installName = "LCDSirPlus-Qualification-$id"
    if (-not $Machine) { $installName += [char]::ConvertFromUtf32(0x1f680) }
    $installRoot = if ($Machine) { Join-Path $env:ProgramFiles $installName } else { Join-Path ([IO.Path]::GetTempPath()) $installName }
    if ([IO.Directory]::Exists($installRoot) -or [IO.File]::Exists($installRoot)) { throw 'exact GUID qualification install root is already occupied' }
    if (-not $Machine) {
        $ansi = [Text.Encoding]::GetEncoding([Globalization.CultureInfo]::CurrentCulture.TextInfo.ANSICodePage)
        if ($ansi.GetString($ansi.GetBytes($installRoot)) -ceq $installRoot) { throw 'current-user qualification path must not round-trip through the ANSI code page' }
    }
    $scopeSwitch = if ($Machine) { '/ALLUSERS' } else { '/CURRENTUSER' }
    $programs = if ($Machine) { Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\LCDSirPlus' } else { Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\LCDSirPlus' }
    $desktop = if ($Machine) { Join-Path $env:PUBLIC 'Desktop\LCDSirPlus.lnk' } else { Join-Path ([Environment]::GetFolderPath('Desktop')) 'LCDSirPlus.lnk' }
    $programsExisted = [IO.Directory]::Exists($programs)
    $appShortcut = Join-Path $programs 'LCDSirPlus.lnk'
    $uninstallShortcut = Join-Path $programs 'Uninstall LCDSirPlus.lnk'
    foreach ($shortcut in @($appShortcut, $uninstallShortcut, $desktop)) {
        if ([IO.File]::Exists($shortcut) -or [IO.Directory]::Exists($shortcut)) { throw "qualification requires exact final shortcut path to be unused: $shortcut" }
    }
    $logRoot = Join-Path ([IO.Path]::GetTempPath()) ("LCDSirPlus-Qualification-Logs-$id")
    if ([IO.Directory]::Exists($logRoot) -or [IO.File]::Exists($logRoot)) { throw 'qualification log root unexpectedly exists' }
    [IO.Directory]::CreateDirectory($logRoot) | Out-Null
    [IO.File]::WriteAllLines((Join-Path $logRoot 'qualification-evidence.txt'), @(
        "SetupSha256=$ExpectedSetupHash", "SourceIdentity=$ExpectedIdentity"), [Text.Encoding]::ASCII)
    $setupArguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', $scopeSwitch, ('/DIR="' + $installRoot + '"'))
    $foreignTaskCreated = $false
    $userDataCreated = -not $userDataExisted
    $sentinelIdentity = $null
    $sentinelBytes = [Text.Encoding]::UTF8.GetBytes("# INNO-QUALIFICATION-SENTINEL-$id`r`n")
    $installRootCreated = $false
    $oppositeRoot = $null
    $oppositeRootCreated = $false
    $originalTaskXml = $null
    $qualificationFailure = $null
    try {
        [IO.Directory]::CreateDirectory($sentinelDir) | Out-Null
        Assert-SafeLocalDirectory -Path $sentinelDir | Out-Null
        $sentinelStream = [IO.File]::Open($sentinelPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $sentinelStream.Write($sentinelBytes, 0, $sentinelBytes.Length) }
        finally { $sentinelStream.Dispose() }
        $sentinelIdentity = Assert-RegularSingleLinkFile $sentinelPath
        if ($Machine) {
            $oppositeRoot = Join-Path ([IO.Path]::GetTempPath()) ("LCDSirPlus-Opposite-$id")
            if ([IO.Directory]::Exists($oppositeRoot) -or [IO.File]::Exists($oppositeRoot)) { throw 'exact opposite-scope qualification root is already occupied' }
            [IO.Directory]::CreateDirectory($oppositeRoot) | Out-Null
            $oppositeRootCreated = $true
            $currentUserArguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/CURRENTUSER',
                ('/DIR="' + $oppositeRoot + '"'))
            if ((Invoke-QualificationProcess $Setup (Add-QualificationLog $currentUserArguments $logRoot 'opposite-current-user-install') 'opposite current-user install') -ne 0 -or $null -eq (Get-UninstallRegistration)) {
                throw 'current-user setup failed while qualifying current-user to all-users refusal'
            }
            if ((Invoke-QualificationProcess $Setup (Add-QualificationLog $setupArguments $logRoot 'opposite-all-users-refusal') 'opposite all-users refusal') -eq 0 -or $null -ne (Get-UninstallRegistration -Machine)) {
                throw 'all-users setup accepted the same user SID in the opposite current-user scope'
            }
            $oppositeRegistration = Get-UninstallRegistration
            if ($oppositeRegistration.UninstallString -cnotmatch '^"([^"]+\.exe)"$' -or
                (Invoke-QualificationProcess $Matches[1] (Add-QualificationLog @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') $logRoot 'opposite-current-user-uninstall') 'opposite current-user uninstall') -ne 0) {
                throw 'current-user opposite-scope fixture cleanup uninstall failed'
            }
            if ($null -ne (Get-UninstallRegistration) -or $null -ne (Get-StartupTaskXml -AllowMissing)) {
                throw 'current-user opposite-scope fixture was not removed'
            }
        }

        [IO.Directory]::CreateDirectory($installRoot) | Out-Null
        $installRootCreated = $true
        [IO.File]::WriteAllText((Join-Path $installRoot 'Install.ps1'), 'legacy fixture')
        if ((Invoke-QualificationProcess $Setup (Add-QualificationLog $setupArguments $logRoot 'legacy-destination-refusal') 'legacy destination refusal') -eq 0 -or $null -ne (Get-UninstallRegistration -Machine:$Machine)) {
            throw 'setup accepted a legacy PowerShell installation destination'
        }
        Remove-Item -LiteralPath (Join-Path $installRoot 'Install.ps1') -Force
        [IO.Directory]::Delete($installRoot)

        $service = New-Object -ComObject 'Schedule.Service'
        $service.Connect()
        $taskRoot = $service.GetFolder('\')
        $definition = $service.NewTask(0)
        $currentUser = [Security.Principal.WindowsIdentity]::GetCurrent().Name
        $definition.RegistrationInfo.Source = 'LCDSirPlus qualification foreign fixture'
        $definition.RegistrationInfo.Description = 'Foreign collision fixture'
        $definition.Principal.UserId = $currentUser
        $definition.Principal.LogonType = 3
        $definition.Principal.RunLevel = 0
        $foreignTrigger = $definition.Triggers.Create(9)
        $foreignTrigger.UserId = $currentUser
        $foreignAction = $definition.Actions.Create(0)
        $foreignAction.Path = Join-Path $env:SystemRoot 'System32\notepad.exe'
        [void]$taskRoot.RegisterTaskDefinition($startupTaskName, $definition, 6, $currentUser, $null, 3)
        $foreignTaskCreated = $true
        if ((Invoke-QualificationProcess $Setup (Add-QualificationLog $setupArguments $logRoot 'foreign-task-refusal') 'foreign task refusal') -eq 0 -or [IO.File]::Exists((Join-Path $installRoot 'lcdsirplus.layout'))) {
            throw 'setup overwrote a foreign scheduled-task collision or copied files before refusal'
        }
        $foreignXml = Get-StartupTaskXml
        $foreignSource = Get-OnlyXmlNode $foreignXml "/*[local-name()='Task']/*[local-name()='RegistrationInfo']/*[local-name()='Source']" 'foreign task source'
        if ($foreignSource.InnerText -cne 'LCDSirPlus qualification foreign fixture') { throw 'setup changed the foreign scheduled task' }
        $taskRoot.DeleteTask($startupTaskName, 0)
        $foreignTaskCreated = $false

        if ((Invoke-QualificationProcess $Setup (Add-QualificationLog $setupArguments $logRoot 'install') 'qualification install') -ne 0) { throw 'qualification install failed' }
        $registration = Get-UninstallRegistration -Machine:$Machine
        Assert-QualificationInstall -InstallRoot $installRoot -Setup $Setup -Registration $registration -ScopeSwitch $scopeSwitch -ExpectedIdentity $ExpectedIdentity -Machine:$Machine
        Assert-QualificationSentinel $sentinelPath $sentinelBytes $sentinelIdentity
        $originalTaskXml = $taskRoot.GetTask($startupTaskName).Xml
        $tamperedTask = [xml]$originalTaskXml
        $tamperedTask.Task.RegistrationInfo.Description = 'LCDSirPlus qualification tamper fixture'
        [void]$taskRoot.RegisterTask($startupTaskName, $tamperedTask.OuterXml, 6, $currentUserSid, $null, 3)
        $layoutHash = (Get-FileHash -LiteralPath (Join-Path $installRoot 'lcdsirplus.layout') -Algorithm SHA256).Hash
        if ((Invoke-QualificationProcess $Setup (Add-QualificationLog $setupArguments $logRoot 'tampered-repair-refusal') 'tampered repair refusal') -eq 0 -or
            (Get-FileHash -LiteralPath (Join-Path $installRoot 'lcdsirplus.layout') -Algorithm SHA256).Hash -cne $layoutHash) {
            throw 'repair did not refuse the tampered startup task before changing installed files'
        }
        Assert-QualificationSentinel $sentinelPath $sentinelBytes $sentinelIdentity
        if ($registration.UninstallString -cnotmatch '^"([^"]+\.exe)"$' -or
            (Invoke-QualificationProcess $Matches[1] (Add-QualificationLog @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') $logRoot 'tampered-uninstall-refusal') 'tampered uninstall refusal') -eq 0 -or
            $null -eq (Get-UninstallRegistration -Machine:$Machine) -or
            -not [IO.File]::Exists((Join-Path $installRoot 'LCDSirPlus.exe'))) {
            throw 'uninstall did not refuse the tampered startup task before removing application state'
        }
        Assert-QualificationSentinel $sentinelPath $sentinelBytes $sentinelIdentity
        $tamperedXml = Get-StartupTaskXml
        $tamperedDescription = Get-OnlyXmlNode $tamperedXml "/*[local-name()='Task']/*[local-name()='RegistrationInfo']/*[local-name()='Description']" 'tampered task description'
        if ($tamperedDescription.InnerText -cne 'LCDSirPlus qualification tamper fixture') {
            throw 'repair or uninstall changed the refused tampered task'
        }
        [void]$taskRoot.RegisterTask($startupTaskName, $originalTaskXml, 6, $currentUserSid, $null, 3)
        if ($taskRoot.GetTask($startupTaskName).Xml -cne $originalTaskXml) {
            throw 'qualification could not restore the exact pre-tamper startup task XML'
        }
        if ($null -ne (Get-UninstallRegistration -Machine:(-not $Machine))) { throw 'installer wrote the opposite-scope uninstall registration' }
        if ($Machine) {
            $oppositeArguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/CURRENTUSER', ('/DIR="' + $oppositeRoot + '"'))
            if ((Invoke-QualificationProcess $Setup (Add-QualificationLog $oppositeArguments $logRoot 'opposite-scope-registration-refusal') 'opposite-scope registration refusal') -eq 0 -or $null -ne (Get-UninstallRegistration)) {
                throw 'setup accepted an opposite-scope registration collision'
            }
        }

        $readme = Join-Path $installRoot 'README.md'
        $ownedHash = (Get-FileHash -LiteralPath $readme -Algorithm SHA256).Hash
        [IO.File]::AppendAllText($readme, 'CORRUPTED')
        $installedMarker = Get-Item -LiteralPath (Join-Path $installRoot 'SOURCE-COMMIT.txt') -Force
        $installedMarker.IsReadOnly = $false
        [IO.File]::WriteAllText($installedMarker.FullName, 'WORKTREE-' + ('0' * 64) + "`n", [Text.Encoding]::ASCII)
        $modifyPattern = '^"([^"]+)" (/(?:CURRENTUSER|ALLUSERS))$'
        if ($registration.ModifyPath -cnotmatch $modifyPattern) { throw 'ModifyPath cannot be invoked safely' }
        if ((Invoke-QualificationProcess $Matches[1] (Add-QualificationLog @($Matches[2], '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') $logRoot 'cached-repair') 'cached ModifyPath repair') -ne 0) {
            throw 'cached ModifyPath repair failed'
        }
        $registration = Get-UninstallRegistration -Machine:$Machine
        Assert-QualificationInstall -InstallRoot $installRoot -Setup $Setup -Registration $registration -ScopeSwitch $scopeSwitch -ExpectedIdentity $ExpectedIdentity -Machine:$Machine
        if ((Get-FileHash -LiteralPath $readme -Algorithm SHA256).Hash -cne $ownedHash -or
            -not [IO.File]::Exists($sentinelPath)) {
            throw 'repair did not restore owned content or preserve LocalAppData configuration'
        }
        Assert-QualificationSentinel $sentinelPath $sentinelBytes $sentinelIdentity

        if ($registration.UninstallString -cnotmatch '^"([^"]+\.exe)"$') { throw 'uninstall registration command is unsafe' }
        if ((Invoke-QualificationProcess $Matches[1] (Add-QualificationLog @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') $logRoot 'uninstall') 'qualification uninstall') -ne 0) { throw 'qualification uninstall failed' }
        $uninstallRootWait = [Diagnostics.Stopwatch]::StartNew()
        while ([IO.Directory]::Exists($installRoot) -and $uninstallRootWait.Elapsed.TotalSeconds -lt 15) {
            [Threading.Thread]::Sleep(100)
        }
        if ([IO.Directory]::Exists($installRoot)) {
            $residual = @(Get-ChildItem -LiteralPath $installRoot -Force -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
                [IO.Path]::GetRelativePath($installRoot, $_.FullName) + $(if ($_.PSIsContainer) { '\' } else { '' })
            })
            if ($residual.Count -eq 0) { $residual = @('<empty root>') }
            throw "qualification uninstall root remained after 15 seconds; residual: $($residual -join ', ')"
        }
        Assert-QualificationSentinel $sentinelPath $sentinelBytes $sentinelIdentity
        if ($null -ne (Get-UninstallRegistration -Machine:$Machine) -or $null -ne (Get-StartupTaskXml -AllowMissing) -or
            [IO.Directory]::Exists($installRoot) -or [IO.File]::Exists($appShortcut) -or
            [IO.File]::Exists($uninstallShortcut) -or [IO.File]::Exists($desktop)) {
            throw 'uninstall did not remove registration/task/payload/shortcuts or preserve LocalAppData'
        }
    }
    catch { $qualificationFailure = $_ }
    finally {
        Invoke-BestEffortCleanup 'installed registration' {
            $cleanupRegistration = Get-UninstallRegistration -Machine:$Machine
            if ($null -ne $cleanupRegistration) {
                if (-not (Test-RegistrationTargetsRoot $cleanupRegistration $installRoot)) { throw 'registration ownership is uncertain' }
                if ($null -ne $originalTaskXml) {
                    $cleanupService = New-Object -ComObject 'Schedule.Service'
                    $cleanupService.Connect()
                    [void]$cleanupService.GetFolder('\').RegisterTask($startupTaskName, $originalTaskXml, 6, $currentUserSid, $null, 3)
                }
                if ($cleanupRegistration.UninstallString -match '^"([^"]+\.exe)"$') {
                    [void](Invoke-QualificationProcess $Matches[1] (Add-QualificationLog @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') $logRoot 'cleanup-main-uninstall') 'cleanup main uninstall')
                }
                $cleanupRegistration = Get-UninstallRegistration -Machine:$Machine
                if ($null -ne $cleanupRegistration) {
                    if (-not (Test-RegistrationTargetsRoot $cleanupRegistration $installRoot)) { throw 'remaining registration ownership is uncertain' }
                    Remove-ExactUninstallRegistration -Machine:$Machine
                }
            }
        }
        Invoke-BestEffortCleanup 'current-user opposite-scope registration' {
            $cleanupRegistration = Get-UninstallRegistration
            if ($Machine -and $null -ne $cleanupRegistration) {
                if (-not (Test-RegistrationTargetsRoot $cleanupRegistration $oppositeRoot)) { throw 'opposite-scope registration ownership is uncertain' }
                if ($cleanupRegistration.UninstallString -match '^"([^"]+\.exe)"$') {
                    [void](Invoke-QualificationProcess $Matches[1] (Add-QualificationLog @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') $logRoot 'cleanup-opposite-uninstall') 'cleanup opposite uninstall')
                }
                $cleanupRegistration = Get-UninstallRegistration
                if ($null -ne $cleanupRegistration) {
                    if (-not (Test-RegistrationTargetsRoot $cleanupRegistration $oppositeRoot)) { throw 'remaining opposite-scope registration ownership is uncertain' }
                    Remove-ExactUninstallRegistration
                }
            }
        }
        Invoke-BestEffortCleanup 'scheduled task' {
            $remainingTask = Get-StartupTaskXml -AllowMissing
            if ($null -ne $remainingTask) {
                if ($foreignTaskCreated) {
                    $source = Get-OnlyXmlNode $remainingTask "/*[local-name()='Task']/*[local-name()='RegistrationInfo']/*[local-name()='Source']" 'cleanup foreign task source'
                    if ($source.InnerText -cne 'LCDSirPlus qualification foreign fixture') { throw 'foreign fixture task ownership is uncertain' }
                }
                else { Assert-StartupTaskContract -InstallRoot $installRoot -ExpectedSid $currentUserSid }
                $service = New-Object -ComObject 'Schedule.Service'
                $service.Connect()
                $service.GetFolder('\').DeleteTask($startupTaskName, 0)
            }
        }
        Invoke-BestEffortCleanup 'qualification LocalAppData' {
            if ([IO.File]::Exists($sentinelPath)) {
                Assert-QualificationSentinel $sentinelPath $sentinelBytes $sentinelIdentity
                [IO.File]::Delete($sentinelPath)
            }
            if ([IO.Directory]::Exists($sentinelDir)) {
                Assert-SafeLocalDirectory -Path $sentinelDir | Out-Null
                if (@(Get-ChildItem -LiteralPath $sentinelDir -Force).Count -ne 0) { throw 'qualification sentinel directory contains an unowned item' }
                [IO.Directory]::Delete($sentinelDir)
            }
            if ($userDataCreated -and [IO.Directory]::Exists($userData)) {
                Assert-SafeLocalDirectory -Path $userData | Out-Null
                if (@(Get-ChildItem -LiteralPath $userData -Force).Count -ne 0) { throw 'harness-created user-data root contains an unowned item' }
                [IO.Directory]::Delete($userData)
            }
        }
        Invoke-BestEffortCleanup 'qualification install root' {
            if ($installRootCreated -and [IO.Directory]::Exists($installRoot)) {
                Assert-SafeTree $installRoot
                Remove-Item -LiteralPath $installRoot -Recurse -Force
            }
        }
        Invoke-BestEffortCleanup 'opposite-scope install root' {
            if ($oppositeRootCreated -and [IO.Directory]::Exists($oppositeRoot)) {
                Assert-SafeTree $oppositeRoot
                Remove-Item -LiteralPath $oppositeRoot -Recurse -Force
            }
        }
        Invoke-BestEffortCleanup 'qualification shortcuts' {
            Remove-QualificationShortcut $appShortcut $installRoot
            Remove-QualificationShortcut $uninstallShortcut $installRoot -Uninstaller
            Remove-QualificationShortcut $desktop $installRoot
            if (-not $programsExisted -and [IO.Directory]::Exists($programs) -and @(Get-ChildItem -LiteralPath $programs -Force).Count -eq 0) {
                [IO.Directory]::Delete($programs)
            }
        }
        Write-Host "Qualification logs retained: $logRoot"
    }

    $verificationFailure = $null
    try {
        if ($null -ne (Get-UninstallRegistration) -or $null -ne (Get-UninstallRegistration -Machine) -or
            $null -ne (Get-StartupTaskXml -AllowMissing) -or [IO.Directory]::Exists($installRoot) -or
            ($null -ne $oppositeRoot -and [IO.Directory]::Exists($oppositeRoot)) -or
            [IO.File]::Exists($appShortcut) -or [IO.File]::Exists($uninstallShortcut) -or [IO.File]::Exists($desktop) -or
            [IO.File]::Exists($sentinelPath) -or [IO.Directory]::Exists($sentinelDir) -or
            (Test-ExactProcessRunning (Join-Path $installRoot 'LCDSirPlus.exe'))) {
            throw 'final lifecycle registration/task/install/shortcut/cache/sentinel/process cleanup is incomplete'
        }
        if ($userDataExisted) {
            if (-not [IO.Directory]::Exists($userData)) { throw 'pre-existing canonical user-data root was removed' }
            Assert-MetadataEqual $userDataBefore @(Get-UserDataMetadata $userData) 'pre-existing canonical user data'
        }
        elseif ([IO.Directory]::Exists($userData) -or [IO.File]::Exists($userData)) { throw 'harness-created canonical user-data root remains' }
        $legacyAfter = Get-ScheduledTaskXmlText -Name '\LCDSirPlus' -AllowMissing
        if (($null -eq $legacyTaskXml) -ne ($null -eq $legacyAfter) -or
            ($null -ne $legacyTaskXml -and $legacyAfter -cne $legacyTaskXml)) { throw 'legacy LCDSirPlus task changed' }
        Write-Host "Legacy task after: present=$($null -ne $legacyAfter) sha256=$legacyTaskHash"
        Write-Host "User-data metadata preserved: entries=$($userDataBefore.Count) preExistingRoot=$userDataExisted"
    }
    catch { $verificationFailure = $_ }
    if ($null -ne $qualificationFailure) {
        if ($null -ne $verificationFailure) { throw "Qualification failed: $qualificationFailure Final cleanup verification also failed: $verificationFailure" }
        throw $qualificationFailure
    }
    if ($null -ne $verificationFailure) { throw $verificationFailure }
    if ($Machine) { Write-Host 'ALL-USERS INSTALL/REPAIR/UNINSTALL QUALIFICATION PASSED' -ForegroundColor Green }
    else { Write-Host 'CURRENT-USER INSTALL/REPAIR/UNINSTALL QUALIFICATION PASSED' -ForegroundColor Green }
}

function Test-LifecycleCleanupStatic {
    $source = [IO.File]::ReadAllText($PSCommandPath, [Text.Encoding]::UTF8)
    $lifecycle = [regex]::Match($source, '(?s)function Invoke-InstallerLifecycleQualification.*?(?=function Test-LifecycleCleanupStatic)').Value
    if ([string]::IsNullOrWhiteSpace($lifecycle)) { throw 'lifecycle function could not be isolated' }
    foreach ($required in @(
        "Get-ScheduledTaskXmlText -Name '\LCDSirPlus'",
        "('.qualification-' + `$id)",
        'Assert-QualificationSentinel $sentinelPath $sentinelBytes $sentinelIdentity',
        'Assert-QualificationInstall -InstallRoot $installRoot -Setup $Setup',
        '"SetupSha256=$ExpectedSetupHash", "SourceIdentity=$ExpectedIdentity"',
        'Assert-MetadataEqual $userDataBefore',
        '[IO.Directory]::Delete($installRoot)',
        'Remove-QualificationShortcut $appShortcut $installRoot',
        'if ($userDataCreated -and [IO.Directory]::Exists($userData))')) {
        if ($lifecycle.IndexOf($required, [StringComparison]::Ordinal) -lt 0) { throw "ownership-safe lifecycle cleanup contract missing: $required" }
    }
    foreach ($forbidden in @(
        'qualification requires absent canonical LCDSirPlus user data',
        'Remove-Item -LiteralPath $userData -Recurse',
        'Remove-Item -LiteralPath $programs -Recurse',
        "DeleteTask('LCDSirPlus'",
        'DeleteTask("LCDSirPlus"')) {
        if ($lifecycle.IndexOf($forbidden, [StringComparison]::OrdinalIgnoreCase) -ge 0) { throw "over-broad lifecycle cleanup contract remains: $forbidden" }
    }
    foreach ($required in @(
        '$ansiExpected = $ansi.GetString($ansi.GetBytes($expected))',
        'Test-RegistrationTargetsRoot $cleanupRegistration $installRoot',
        'Remove-ExactUninstallRegistration -Machine:$Machine',
        '$runLevels.Count -eq 1')) {
        if ($source.IndexOf($required, [StringComparison]::Ordinal) -lt 0) { throw "ownership-safe cleanup seam missing: $required" }
    }
    $uninstallExit = $lifecycle.IndexOf("throw 'qualification uninstall failed'", [StringComparison]::Ordinal)
    $rootWait = $lifecycle.IndexOf('$uninstallRootWait = [Diagnostics.Stopwatch]::StartNew()', [StringComparison]::Ordinal)
    $aggregate = $lifecycle.IndexOf('if ($null -ne (Get-UninstallRegistration -Machine:$Machine)', [StringComparison]::Ordinal)
    if ($uninstallExit -lt 0 -or $rootWait -le $uninstallExit -or $aggregate -le $rootWait) {
        throw 'bounded uninstall-root convergence ordering contract missing'
    }
    $rootConvergence = $lifecycle.Substring($rootWait, $aggregate - $rootWait)
    foreach ($required in @(
        'while ([IO.Directory]::Exists($installRoot) -and $uninstallRootWait.Elapsed.TotalSeconds -lt 15)',
        '[Threading.Thread]::Sleep(100)',
        'Get-ChildItem -LiteralPath $installRoot -Force -Recurse -ErrorAction SilentlyContinue',
        'qualification uninstall root remained after 15 seconds; residual:')) {
        if ($rootConvergence.IndexOf($required, [StringComparison]::Ordinal) -lt 0) { throw "bounded uninstall-root convergence contract missing: $required" }
    }
    foreach ($forbidden in @('Get-UninstallRegistration', 'Get-StartupTaskXml', '$appShortcut', '$uninstallShortcut', '$desktop')) {
        if ($rootConvergence.IndexOf($forbidden, [StringComparison]::Ordinal) -ge 0) { throw "uninstall convergence poll includes non-root state: $forbidden" }
    }
}

function Test-ReleaseOutputTransactions {
    param([string]$Root)
    $output = Join-Path $Root 'release'
    [IO.Directory]::CreateDirectory($output) | Out-Null
    [IO.File]::WriteAllText((Join-Path $output 'old.txt'), 'old')

    $beforeMove = New-ReleaseOutputTransaction -Output $output
    if (-not [IO.File]::Exists((Join-Path $output 'old.txt')) -or
        [IO.Directory]::Exists($beforeMove.Prior) -or
        @(Get-ChildItem -LiteralPath $beforeMove.Stage -Force).Count -ne 0) {
        throw 'new release transaction displaced canonical output or did not create an empty stage first'
    }
    [IO.File]::WriteAllText((Join-Path $beforeMove.Stage 'new.txt'), 'new')
    Expect-Failure { Complete-ReleaseOutputTransaction -Transaction $beforeMove -TestFailAt BeforeMove } 'release failure before canonical move unexpectedly succeeded'
    if (-not [IO.File]::Exists((Join-Path $output 'old.txt')) -or [IO.Directory]::Exists($beforeMove.Prior)) {
        throw 'release failure before canonical move changed canonical output'
    }
    Remove-Item -LiteralPath $beforeMove.Stage -Recurse -Force

    $afterMove = New-ReleaseOutputTransaction -Output $output
    [IO.File]::WriteAllText((Join-Path $afterMove.Stage 'new.txt'), 'new')
    Expect-Failure { Complete-ReleaseOutputTransaction -Transaction $afterMove -TestFailAt AfterMove } 'release failure after canonical move unexpectedly succeeded'
    if (-not [IO.File]::Exists((Join-Path $output 'old.txt')) -or [IO.Directory]::Exists($afterMove.Prior)) {
        throw 'release failure after canonical move did not restore canonical output'
    }
    Remove-Item -LiteralPath $afterMove.Stage -Recurse -Force

    $successful = New-ReleaseOutputTransaction -Output $output
    [IO.File]::WriteAllText((Join-Path $successful.Stage 'current.txt'), 'current')
    Complete-ReleaseOutputTransaction -Transaction $successful
    if ([IO.Directory]::Exists($successful.Stage) -or [IO.Directory]::Exists($successful.Prior) -or
        -not [IO.File]::Exists((Join-Path $output 'current.txt')) -or [IO.File]::Exists((Join-Path $output 'old.txt'))) {
        throw 'successful release promotion did not atomically replace canonical output'
    }
}

function Test-NoticeInventory {
    param([string]$Root)
    $noticeSource = Join-Path $repo 'THIRD_PARTY_LICENSES.txt'
    $bytes = [IO.File]::ReadAllBytes($noticeSource)
    foreach ($byte in $bytes) {
        if ($byte -gt 0x7f -or ($byte -lt 0x20 -and $byte -notin @(0x09, 0x0a, 0x0d))) {
            throw 'third-party notice contains non-ASCII or unsupported control bytes'
        }
    }
    $build = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'Build.ps1'), [Text.Encoding]::UTF8)
    if ($build.IndexOf("'THIRD_PARTY_LICENSES.txt'", [StringComparison]::Ordinal) -lt 0) {
        throw 'third-party notice is absent from the release payload inventory'
    }

    $rustup = Get-Command rustup -CommandType Application -ErrorAction SilentlyContinue
    if ($null -eq $rustup) { throw 'NoticeInventory requires an installed rustup-managed Rust 1.97.1 toolchain' }
    $selectedToolchain = [Environment]::GetEnvironmentVariable('RUSTUP_TOOLCHAIN', 'Process')
    if ([string]::IsNullOrWhiteSpace($selectedToolchain)) {
        $selectedToolchain = '1.97.1-x86_64-pc-windows-msvc'
        $installed = @(& $rustup.Source toolchain list | ForEach-Object { ($_ -split '\s+')[0] })
        if ($selectedToolchain -cnotin $installed) {
            throw 'NoticeInventory requires the installed exact 1.97.1-x86_64-pc-windows-msvc toolchain alias'
        }
    }
    $verbose = (& $rustup.Source run $selectedToolchain rustc --version --verbose | Out-String).Replace("`r", '')
    if ($LASTEXITCODE -ne 0 -or $verbose -notmatch '(?m)^release: 1\.97\.1$' -or
        $verbose -notmatch '(?m)^commit-hash: 8bab26f4f68e0e26f0bb7960be334d5b520ea452$' -or
        $verbose -notmatch '(?m)^host: x86_64-pc-windows-msvc$') {
        throw "NoticeInventory selected toolchain '$selectedToolchain' is not the pinned Rust provenance"
    }
    $toolchainBefore = [Environment]::GetEnvironmentVariable('RUSTUP_TOOLCHAIN', 'Process')
    try {
        $env:RUSTUP_TOOLCHAIN = $selectedToolchain
        $metadataText = (& cargo metadata --format-version 1 --locked --offline --filter-platform x86_64-pc-windows-msvc | Out-String)
        if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed while deriving the notice closure' }
    }
    finally {
        if ($null -eq $toolchainBefore) { Remove-Item Env:\RUSTUP_TOOLCHAIN -ErrorAction SilentlyContinue }
        else { $env:RUSTUP_TOOLCHAIN = $toolchainBefore }
    }
    $metadata = $metadataText | ConvertFrom-Json
    $expectedPackages = @($metadata.packages | Where-Object { $_.name -cne 'lcdsirplus' } | ForEach-Object { "$($_.name) $($_.version)" })
    $expectedPackages += @('serde_derive 1.0.229', 'syn 3.0.4')
    $expectedPackages = @(Get-OrdinalSorted @($expectedPackages | Sort-Object -Unique))
    $noticeText = [Text.Encoding]::ASCII.GetString($bytes)
    $declaredPackages = @([regex]::Matches($noticeText, '(?m)^([A-Za-z0-9_-]+) ([0-9]+\.[0-9]+\.[0-9]+)$') | ForEach-Object { $_.Groups[1].Value + ' ' + $_.Groups[2].Value })
    $declaredPackages = @(Get-OrdinalSorted @($declaredPackages | Sort-Object -Unique))
    if ($declaredPackages.Count -ne $expectedPackages.Count) { throw 'third-party notice package/version inventory mismatch' }
    for ($index = 0; $index -lt $expectedPackages.Count; $index++) {
        if ($declaredPackages[$index] -cne $expectedPackages[$index]) { throw 'third-party notice package/version inventory mismatch' }
    }
    $lockText = [IO.File]::ReadAllText((Join-Path $repo 'Cargo.lock'), [Text.Encoding]::UTF8)
    foreach ($package in $expectedPackages) {
        $parts = $package.Split(' ')
        $block = [regex]::Match($lockText, "(?ms)^\[\[package\]\]\s+name = `"$([regex]::Escape($parts[0]))`"\s+version = `"$([regex]::Escape($parts[1]))`".*?(?=^\[\[package\]\]|\z)")
        if (-not $block.Success) { throw "notice package is absent from Cargo.lock: $package" }
        $checksum = [regex]::Match($block.Value, '(?m)^checksum = "([0-9a-f]{64})"$')
        if ($checksum.Success -and $noticeText.IndexOf($checksum.Groups[1].Value, [StringComparison]::Ordinal) -lt 0) {
            throw "Cargo.lock checksum is absent from third-party notice: $package"
        }
    }

    $fixture = Join-Path $Root 'notice-manifest'
    [IO.Directory]::CreateDirectory($fixture) | Out-Null
    $notice = Join-Path $fixture 'THIRD_PARTY_LICENSES.txt'
    Copy-Item -LiteralPath $noticeSource -Destination $notice
    $identity = Assert-RegularSingleLinkFile $notice
    $hash = (Get-FileHash -LiteralPath $notice -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText((Join-Path $fixture 'PACKAGE-MANIFEST.txt'),
        "${hash}`t$($identity.Size)`tTHIRD_PARTY_LICENSES.txt`n", (New-Object Text.UTF8Encoding($false)))
    $entries = @(Read-VerifiedManifest $fixture)
    Assert-ThirdPartyNoticeManifest -Root $fixture -Entries $entries
}

function Test-SourcePrivacy {
    param([string]$Root)
    $scannerBytes = [IO.File]::ReadAllBytes($PSCommandPath)
    Assert-PrivateBytesAbsent -Bytes $scannerBytes -Needles @() -SensitivePackage `
        -RelativePath 'LCDSirPlus-0.3.0-source/scripts/Package-Test.ps1'
    Expect-Failure {
        Assert-PrivateBytesAbsent -Bytes $scannerBytes -Needles @() -SensitivePackage -RelativePath 'fixture/Package-Test.ps1'
    } 'scanner test literals were excluded outside their exact source path'

    $fixture = Join-Path $Root 'source-secret.zip'
    $stream = [IO.File]::Open($fixture, [IO.FileMode]::CreateNew)
    try {
        $zip = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
        try {
            $entry = $zip.CreateEntry('LCDSirPlus-0.3.0-source/src/leak.txt')
            $writer = New-Object IO.StreamWriter($entry.Open(), [Text.Encoding]::UTF8)
            try { $writer.Write(('github' + '_pat_' + ('A' * 24))) }
            finally { $writer.Dispose() }
        }
        finally { $zip.Dispose() }
    }
    finally { $stream.Dispose() }
    Expect-Failure { Assert-ArchivePrivacy -Archive $fixture -Needles @() -SensitivePackage } 'synthetic source token was accepted'
}

if ($Mode -in @('CurrentUserLifecycle', 'AllUsersQualification')) {
    if (-not $ConfirmSystemMutation) { throw "$Mode requires explicit -ConfirmSystemMutation" }
    if ([string]::IsNullOrWhiteSpace($ExpectedSetupSha256) -or [string]::IsNullOrWhiteSpace($ExpectedSourceIdentity)) {
        throw "$Mode requires -ExpectedSetupSha256 and -ExpectedSourceIdentity"
    }
    if ([string]::IsNullOrWhiteSpace($SetupPath) -or -not [IO.Path]::IsPathRooted($SetupPath)) {
        throw "$Mode requires an absolute -SetupPath"
    }
    $qualificationSetup = [IO.Path]::GetFullPath($SetupPath)
    if ([IO.Path]::GetFileName($qualificationSetup) -cne 'LCDSirPlus-0.3.0-win-x64-setup.exe') {
        throw 'qualification setup must have the exact release filename'
    }
    Write-Host "Qualification setup SHA-256: $ExpectedSetupSha256"
    Write-Host "Qualification source identity: $ExpectedSourceIdentity"
    [void](Test-InstallerStatic -SetupExe $qualificationSetup -IssPath (Join-Path $repo 'packaging\LCDSirPlus.iss') -PayloadRoot $InstallerPayloadDir -ExpectedSetupHash $ExpectedSetupSha256 -ExpectedIdentity $ExpectedSourceIdentity)
    Invoke-InstallerLifecycleQualification -Setup $qualificationSetup -ExpectedSetupHash $ExpectedSetupSha256 `
        -ExpectedIdentity $ExpectedSourceIdentity -Machine:($Mode -eq 'AllUsersQualification')
    return
}

if ($Mode -eq 'InstallerStatic') {
    if ([string]::IsNullOrWhiteSpace($ArtifactDir) -or [string]::IsNullOrWhiteSpace($InstallerPayloadDir) -or
        [string]::IsNullOrWhiteSpace($ExpectedSetupSha256) -or [string]::IsNullOrWhiteSpace($ExpectedSourceIdentity)) {
        throw 'InstallerStatic mode requires ArtifactDir, InstallerPayloadDir, ExpectedSetupSha256, and ExpectedSourceIdentity'
    }
    $qualificationArtifactRoot = Assert-SafeLocalDirectory -Path $ArtifactDir
    $qualificationSetup = Join-Path $qualificationArtifactRoot 'LCDSirPlus-0.3.0-win-x64-setup.exe'
    [void](Test-InstallerStatic -SetupExe $qualificationSetup -IssPath (Join-Path $repo 'packaging\LCDSirPlus.iss') -PayloadRoot $InstallerPayloadDir -ExpectedSetupHash $ExpectedSetupSha256 -ExpectedIdentity $ExpectedSourceIdentity)
    Write-Host 'PACKAGE TEST PASSED: InstallerStatic' -ForegroundColor Green
    return
}

if ($Mode -ne 'All') {
    $focusedRoot = Join-Path ([IO.Path]::GetTempPath()) ('lcdsirplus-package-focused-' + [Guid]::NewGuid().ToString('N'))
    [IO.Directory]::CreateDirectory($focusedRoot) | Out-Null
    $env:LCDSIRPLUS_PACKAGE_TEST = '1'
    try {
        if ($Mode -eq 'LifecycleCleanupStatic') { Test-LifecycleCleanupStatic }
        elseif ($Mode -eq 'ReleaseTransaction') { Test-ReleaseOutputTransactions $focusedRoot }
        elseif ($Mode -eq 'SourcePrivacy') { Test-SourcePrivacy $focusedRoot }
        else { Test-NoticeInventory $focusedRoot }
    }
    finally {
        Remove-Item Env:\LCDSIRPLUS_PACKAGE_TEST -ErrorAction SilentlyContinue
        if ([IO.Directory]::Exists($focusedRoot)) { Remove-Item -LiteralPath $focusedRoot -Recurse -Force }
    }
    Write-Host "PACKAGE TEST PASSED: $Mode" -ForegroundColor Green
    return
}

if ([string]::IsNullOrWhiteSpace($ArtifactDir) -or [string]::IsNullOrWhiteSpace($ExpectedCommit) -or
    [string]::IsNullOrWhiteSpace($InstallerPayloadDir) -or [string]::IsNullOrWhiteSpace($ExpectedSetupSha256) -or
    [string]::IsNullOrWhiteSpace($ExpectedSourceIdentity)) {
    throw 'All mode requires ArtifactDir, ExpectedCommit, InstallerPayloadDir, ExpectedSetupSha256, and ExpectedSourceIdentity'
}
if ($ExpectedSourceIdentity -cne $ExpectedCommit) { throw 'final package source identity must equal ExpectedCommit' }
$version = '0.3.0'
$artifactRoot = Assert-SafeLocalDirectory -Path $ArtifactDir
$portableName = "LCDSirPlus-$version-win-x64-portable"
$setupName = "LCDSirPlus-$version-win-x64-setup"
$sourceName = "LCDSirPlus-$version-source"
$checksumName = "LCDSirPlus-$version-SHA256SUMS.txt"
$portableZip = Join-Path $artifactRoot ($portableName + '.zip')
$setupExe = Join-Path $artifactRoot ($setupName + '.exe')
$sourceZip = Join-Path $artifactRoot ($sourceName + '.zip')
$reproducibility = Join-Path $artifactRoot 'REPRODUCIBILITY.json'
$archives = @($portableZip, $sourceZip)
$releaseFiles = @($setupExe, $portableZip, $sourceZip, $reproducibility)

Write-Host '== verify outer checksums and archive names ==' -ForegroundColor Cyan
$sumLines = @([IO.File]::ReadAllLines((Join-Path $artifactRoot $checksumName), [Text.Encoding]::UTF8) | Where-Object { $_.Length -gt 0 })
if ($sumLines.Count -ne 4) { throw 'outer checksum inventory mismatch' }
$lastName = $null
$sumNames = @()
foreach ($line in $sumLines) {
    if ($line -notmatch '^([0-9a-f]{64})\t([0-9]+)\t([^\\/]+\.(?:exe|zip|json))$') { throw 'invalid outer checksum line' }
    $hash = $Matches[1]
    $size = [uint64]$Matches[2]
    $name = $Matches[3]
    $sumNames += $name
    if ($null -ne $lastName -and [StringComparer]::Ordinal.Compare($lastName, $name) -ge 0) { throw 'outer checksums not sorted' }
    $lastName = $name
    $path = Join-Path $artifactRoot $name
    if ((Assert-RegularSingleLinkFile $path).Size -ne $size) { throw 'outer size mismatch' }
    if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $hash) { throw 'outer hash mismatch' }
}
$expectedReleaseNames = @(Get-OrdinalSorted @($releaseFiles | ForEach-Object { [IO.Path]::GetFileName($_) }))
for ($index = 0; $index -lt $expectedReleaseNames.Count; $index++) {
    if ($sumNames[$index] -cne $expectedReleaseNames[$index]) { throw 'outer checksum inventory mismatch' }
}
$artifactNames = @(Get-OrdinalSorted @(Get-ChildItem -LiteralPath $artifactRoot -File -Force | ForEach-Object { $_.Name }))
$expectedArtifactNames = @(Get-OrdinalSorted @($expectedReleaseNames + $checksumName))
if ($artifactNames.Count -ne $expectedArtifactNames.Count) { throw 'release artifact inventory mismatch' }
for ($index = 0; $index -lt $expectedArtifactNames.Count; $index++) {
    if ($artifactNames[$index] -cne $expectedArtifactNames[$index]) { throw 'release artifact inventory mismatch' }
}
if ($sumNames -contains $checksumName) { throw 'outer checksum manifest contains a self-reference' }

$reproducibilityText = [IO.File]::ReadAllText($reproducibility, [Text.Encoding]::UTF8)
$reproducibilityJson = $reproducibilityText | ConvertFrom-Json
if ($reproducibilityJson.schema_version -ne 2 -or $reproducibilityJson.source.root -cne 'source-root' -or
    $reproducibilityJson.source.commit -cne $ExpectedCommit -or
    $reproducibilityJson.tools.rustc.version -cnotmatch '^rustc 1\.97\.1 ' -or
    $reproducibilityJson.tools.rustc.commit -cne '8bab26f4f68e0e26f0bb7960be334d5b520ea452' -or
    $reproducibilityJson.tools.rustc.host -cne 'x86_64-pc-windows-msvc') {
    throw 'REPRODUCIBILITY.json source/toolchain binding mismatch'
}
if ($reproducibilityText -match '"(?:path|cargo_home)"\s*:') { throw 'REPRODUCIBILITY.json contains a physical tool or Cargo-home path field' }

Expect-Failure { Assert-ArchiveMemberNames @('../escape') } 'traversal archive name accepted'
Expect-Failure { Assert-ArchiveMemberNames @('../escape/') } 'traversal archive directory accepted'
Expect-Failure { Assert-ArchiveMemberNames @('root/a', 'root/A') } 'case collision accepted'
Expect-Failure { Assert-ArchiveMemberNames @('root/a', 'root/a') } 'duplicate accepted'
Expect-Failure { Assert-ArchiveMemberNames @('root/file.txt:stream') } 'ADS name accepted'
Expect-Failure { Assert-ArchiveMemberNames @('root/CON.txt') } 'reserved name accepted'

$portableCount = Test-ArchiveInventory $portableZip $portableName
$sourceCount = Test-ArchiveInventory $sourceZip $sourceName

$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('lcdsirplus-package-test-' + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($testRoot) | Out-Null
$env:LCDSIRPLUS_PACKAGE_TEST = '1'

try {
    Write-Host '== notice and source-privacy closure ==' -ForegroundColor Cyan
    Test-NoticeInventory $testRoot
    Test-SourcePrivacy $testRoot

    Write-Host '== release output transaction fixtures ==' -ForegroundColor Cyan
    $preflightLeaf = '.package-test-preflight-' + [Guid]::NewGuid().ToString('N')
    $preflightOutput = Join-Path (Join-Path $repo 'artifacts') $preflightLeaf
    $dirtySentinel = Join-Path $repo ('.package-test-dirty-' + [Guid]::NewGuid().ToString('N'))
    [IO.Directory]::CreateDirectory($preflightOutput) | Out-Null
    [IO.File]::WriteAllBytes((Join-Path $preflightOutput 'canonical.bin'), [byte[]](0, 1, 2, 255))
    $preflightBefore = [Convert]::ToBase64String([IO.File]::ReadAllBytes((Join-Path $preflightOutput 'canonical.bin')))
    try {
        [IO.File]::WriteAllText($dirtySentinel, 'dirty')
        Expect-Failure { & (Join-Path $repo 'scripts\Build.ps1') -OutputDir ('.\artifacts\' + $preflightLeaf) } 'dirty release preflight unexpectedly succeeded'
        $preflightAfter = [Convert]::ToBase64String([IO.File]::ReadAllBytes((Join-Path $preflightOutput 'canonical.bin')))
        $preflightResidue = @(Get-ChildItem -LiteralPath (Split-Path $preflightOutput -Parent) -Directory -Force | Where-Object { $_.Name -like ('.' + $preflightLeaf + '.*') })
        if ($preflightAfter -cne $preflightBefore -or $preflightResidue.Count -ne 0) { throw 'dirty preflight changed canonical output or created release transaction residue' }
    }
    finally {
        if ([IO.File]::Exists($dirtySentinel)) { [IO.File]::Delete($dirtySentinel) }
        if ([IO.Directory]::Exists($preflightOutput)) { Remove-Item -LiteralPath $preflightOutput -Recurse -Force }
    }

    $promotionRoot = Join-Path $testRoot 'promotion'
    [IO.Directory]::CreateDirectory($promotionRoot) | Out-Null
    Test-ReleaseOutputTransactions $promotionRoot

    $junctionTarget = Join-Path $promotionRoot 'junction-target'
    $junction = Join-Path $promotionRoot 'junction'
    [IO.Directory]::CreateDirectory($junctionTarget) | Out-Null
    $junctionSentinel = Join-Path $junctionTarget 'sentinel.txt'
    [IO.File]::WriteAllText($junctionSentinel, 'untouched')
    New-Item -ItemType Junction -Path $junction -Target $junctionTarget | Out-Null
    Expect-Failure { New-ReleaseOutputTransaction -Output (Join-Path $junction 'release') } 'release output beneath an ancestor junction was accepted'
    if ([IO.File]::ReadAllText($junctionSentinel) -cne 'untouched' -or [IO.Directory]::Exists((Join-Path $junctionTarget 'release'))) {
        throw 'ancestor-junction refusal touched its target'
    }
    [IO.Directory]::Delete($junction)

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
    $sourceExtract = Join-Path $testRoot 'source'
    Expand-Archive -LiteralPath $portableZip -DestinationPath $portableExtract
    Expand-Archive -LiteralPath $sourceZip -DestinationPath $sourceExtract
    $portable = Join-Path $portableExtract $portableName
    $source = Join-Path $sourceExtract $sourceName
    foreach ($sourceMember in @('packaging\LCDSirPlus.iss', 'scripts\Acquire-InnoSetup.ps1', 'third_party\InnoSetup\manifest.json', 'third_party\InnoSetup\LICENSE.txt')) {
        Assert-RegularSingleLinkFile (Join-Path $source $sourceMember) | Out-Null
    }
    if ([IO.Directory]::Exists((Join-Path $source 'third_party\InnoSetup\cache'))) { throw 'source package contains the Inno Setup compiler cache' }
    $portableEntries = @(Read-VerifiedManifest $portable)
    [void]@(Read-VerifiedManifest $source)
    $portableFiles = @(
        'LCDSirPlus.exe', 'PresentMon.exe', 'lcdsirplus.txt', 'LICENSE', 'THIRD_PARTY_LICENSES.txt', 'SOURCE-COMMIT.txt', 'README.md', 'modules.md',
        'RELEASE-NOTES.md', 'SECURITY.md', 'docs/CONFIGURATION.md',
        'docs/HARDWARE-ACCEPTANCE.md', 'docs/INSTRUCTION-MANUAL.md',
        'docs/LCDSirPlus-Instruction-Manual.pdf',
        'licenses/PresentMon/LICENSE.txt', 'licenses/PresentMon/THIRD_PARTY.txt'
    )
    Assert-ExpectedInventory $portableEntries $portableFiles 'portable'
    $portableSourceIdentity = Assert-SourceIdentityMarker -Path (Join-Path $portable 'SOURCE-COMMIT.txt') -ExpectedIdentity $ExpectedCommit
    Assert-ThirdPartyNoticeManifest -Root $portable -Entries $portableEntries
    Assert-PresentMonPayload $portable
    $setupSize = Test-InstallerStatic -SetupExe $setupExe -IssPath (Join-Path $source 'packaging\LCDSirPlus.iss') -PayloadRoot $InstallerPayloadDir -ExpectedSetupHash $ExpectedSetupSha256 -ExpectedIdentity $ExpectedSourceIdentity

    Write-Host '== portable clean-extraction smoke ==' -ForegroundColor Cyan
    $exe = Join-Path $portable 'LCDSirPlus.exe'
    $versionOutput = (& $exe --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $versionOutput -cne 'LCDSirPlus 0.3.0') { throw 'portable --version identity failed' }
    $helpOutput = (& $exe --help | Out-String)
    if ($LASTEXITCODE -ne 0 -or $helpOutput -notmatch 'LCDSirPlus\.exe' -or $helpOutput -notmatch '--help, -h' -or $helpOutput -notmatch '--version, -v' -or $helpOutput -match ('lcd' + '([-_ ]?)' + 'for' + 'ge2?')) { throw 'portable --help identity failed' }
    & $exe --validate-config --config (Join-Path $portable 'lcdsirplus.txt') 2>&1 | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'portable --validate-config failed' }
    $diagnostics = Join-Path $testRoot 'diagnostics'
    & $exe --config '\\unreachable.invalid\share\PACKAGE-PRIVATE-SENTINEL.txt' --diagnostics --diagnostic-dir $diagnostics 2>&1 | Out-Host
    if ($LASTEXITCODE -ne 0 -or @(Get-ChildItem -LiteralPath $diagnostics -Filter '*.zip').Count -ne 1) { throw 'portable diagnostics failed' }
    $diagnosticBundle = @(Get-ChildItem -LiteralPath $diagnostics -Filter '*.zip')[0].FullName
    if ([IO.Path]::GetFileName($diagnosticBundle) -notmatch '^LCDSirPlus-Diagnostics-[0-9a-f]+\.zip$') { throw 'diagnostics filename identity mismatch' }
    $diagnosticText = [Text.Encoding]::Latin1.GetString([IO.File]::ReadAllBytes($diagnosticBundle))
    if ($diagnosticText -match 'PACKAGE-PRIVATE-SENTINEL|unreachable\.invalid') { throw 'diagnostics leaked ignored config path' }
    & $exe --hardware-test --backend virtual --duration-secs 1 --config (Join-Path $portable 'lcdsirplus.txt') --diagnostic-dir (Join-Path $testRoot 'logs') 2>&1 | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'portable virtual hardware smoke failed' }

    Write-Host '== source commit binding ==' -ForegroundColor Cyan
    $sourceIdentity = Assert-SourceIdentityMarker -Path (Join-Path $source 'SOURCE-COMMIT.txt') -ExpectedIdentity $ExpectedCommit
    if ($portableSourceIdentity -cne $sourceIdentity) { throw 'portable/source ZIP identity mismatch' }
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
        Assert-ArchivePrivacy -Archive $archive -Needles $needles -SensitivePackage
    }
    Assert-PrivateBytesAbsent -Bytes ([IO.File]::ReadAllBytes($setupExe)) -Needles $needles -SensitivePackage
    Assert-PrivateBytesAbsent -Bytes ([IO.File]::ReadAllBytes($reproducibility)) -Needles $needles
}
finally {
    Remove-Item Env:\LCDSIRPLUS_PACKAGE_TEST -ErrorAction SilentlyContinue
    if ([IO.Directory]::Exists($testRoot)) { Remove-Item -LiteralPath $testRoot -Recurse -Force }
}

Write-Host ("PACKAGE TESTS PASSED: portable={0} setup={1} bytes source={2} members" -f $portableCount, $setupSize, $sourceCount) -ForegroundColor Green
