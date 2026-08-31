#Requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$SourceRoot,
    [Parameter(Mandatory = $true)][string]$OutputDir,
    [Parameter(Mandatory = $true)][ValidatePattern('^[0-9a-f]{40}$')][string]$SourceCommit
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

function Assert-LocalDirectory {
    param([string]$Path, [string]$Label)
    $full = [IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
    if ($full.StartsWith('\\', [StringComparison]::Ordinal)) { throw "$Label must be a local directory" }
    $item = Get-Item -LiteralPath $full -Force
    if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "$Label must be a regular directory"
    }
    $item.FullName.TrimEnd('\', '/')
}

function Test-PathOverlap {
    param([string]$Left, [string]$Right)
    $Left.Equals($Right, [StringComparison]::OrdinalIgnoreCase) -or
        $Left.StartsWith($Right + '\', [StringComparison]::OrdinalIgnoreCase) -or
        $Right.StartsWith($Left + '\', [StringComparison]::OrdinalIgnoreCase)
}

function Get-SourceFiles {
    param([string]$Root)
    $excludedDirectories = @('.git', '.codex', 'artifacts', 'target')
    $files = New-Object 'Collections.Generic.List[object]'
    $pending = New-Object 'Collections.Generic.Stack[IO.DirectoryInfo]'
    $pending.Push((Get-Item -LiteralPath $Root -Force))
    while ($pending.Count -gt 0) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop().FullName -Force) {
            $relative = [IO.Path]::GetRelativePath($Root, $item.FullName).Replace('\', '/')
            if ($item.PSIsContainer) {
                if ($relative.IndexOf('/') -lt 0 -and $excludedDirectories -contains $relative) { continue }
                if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "source contains a reparse directory: $relative" }
                $pending.Push($item)
            } else {
                if ($relative -ceq 'lcdsirplus.local.txt') { continue }
                if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "source contains a reparse file: $relative" }
                $files.Add([pscustomobject]@{ Relative = $relative; FullName = $item.FullName })
            }
        }
    }
    $array = $files.ToArray()
    [Array]::Sort($array, [Comparison[object]]{
        param($left, $right)
        [StringComparer]::Ordinal.Compare($left.Relative, $right.Relative)
    })
    $array
}

function Get-SourceManifest {
    param([object[]]$Files)
    $lines = foreach ($file in $Files) {
        $item = Get-Item -LiteralPath $file.FullName -Force
        $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash`t$($item.Length)`t$($file.Relative)"
    }
    $text = ($lines -join "`n") + "`n"
    $aggregate = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($text))).ToLowerInvariant()
    [pscustomobject]@{ Lines = @($lines); Count = @($lines).Count; Sha256 = $aggregate }
}

function Assert-ManifestsEqual {
    param($Left, $Right, [string]$Message)
    if ($Left.Count -ne $Right.Count -or $Left.Sha256 -cne $Right.Sha256) { throw $Message }
    for ($index = 0; $index -lt $Left.Count; $index++) {
        if ($Left.Lines[$index] -cne $Right.Lines[$index]) { throw $Message }
    }
}

function Get-FileIdentity {
    param([string]$Path)
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "not a regular file: $Path" }
    [ordered]@{
        bytes = [uint64]$item.Length
        sha256 = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    }
}

function Assert-FilesEqual {
    param([string]$Left, [string]$Right, [string]$Message)
    $leftBytes = [IO.File]::ReadAllBytes($Left)
    $rightBytes = [IO.File]::ReadAllBytes($Right)
    if ($leftBytes.Length -ne $rightBytes.Length) { throw $Message }
    for ($index = 0; $index -lt $leftBytes.Length; $index++) {
        if ($leftBytes[$index] -ne $rightBytes[$index]) { throw $Message }
    }
}

function Invoke-ReproducibleBuild {
    param([string]$Root, [string]$Target, [string]$Temp, [string]$Label)
    [IO.Directory]::CreateDirectory($Target) | Out-Null
    [IO.Directory]::CreateDirectory($Temp) | Out-Null
    $remaps = @(
        @($Root, '/workspace'),
        @($Target, '/target'),
        @($cargoHome, '/cargo'),
        @($env:USERPROFILE, '/user'),
        @($env:HOME, '/home'),
        @($Temp, '/tmp')
    )
    $flags = New-Object 'Collections.Generic.List[string]'
    $seen = New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
    foreach ($remap in $remaps) {
        if ([string]::IsNullOrWhiteSpace($remap[0])) { continue }
        $path = [IO.Path]::GetFullPath($remap[0]).TrimEnd('\', '/')
        if ($seen.Add($path)) { $flags.Add("--remap-path-prefix=$path=$($remap[1])") }
    }
    $flags.Add('-C')
    $flags.Add('link-arg=/Brepro')

    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $cargoPath
    $info.WorkingDirectory = $Root
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($argument in @('build', '--release', '--locked', '--offline')) { $info.ArgumentList.Add($argument) }
    $info.Environment.Clear()
    $inherited = @(
        'APPDATA', 'COMSPEC', 'HOMEDRIVE', 'HOMEPATH', 'INCLUDE', 'LIB', 'LIBPATH',
        'LOCALAPPDATA', 'NUMBER_OF_PROCESSORS', 'OS', 'PATH', 'PATHEXT', 'PROCESSOR_ARCHITECTURE',
        'ProgramData', 'ProgramFiles', 'ProgramFiles(x86)', 'ProgramW6432', 'RUSTUP_HOME',
        'SystemDrive', 'SystemRoot', 'USERDOMAIN', 'USERNAME', 'USERPROFILE', 'VCINSTALLDIR',
        'VSINSTALLDIR', 'VCToolsInstallDir', 'WindowsSdkBinPath', 'WindowsSdkDir',
        'WindowsSDKVersion', 'WindowsSdkVerBinPath', 'WINDIR'
    )
    foreach ($name in $inherited) {
        $value = [Environment]::GetEnvironmentVariable($name, 'Process')
        if ($null -ne $value) { $info.Environment[$name] = $value }
    }
    $info.Environment['CARGO_HOME'] = $cargoHome
    $info.Environment['CARGO_INCREMENTAL'] = '0'
    $info.Environment['CARGO_NET_OFFLINE'] = 'true'
    $info.Environment['CARGO_TARGET_DIR'] = $Target
    $info.Environment['CARGO_ENCODED_RUSTFLAGS'] = $flags -join [char]0x1f
    $info.Environment['HOME'] = if ($env:HOME) { $env:HOME } else { $env:USERPROFILE }
    $info.Environment['RUSTC'] = $rustcPath
    $info.Environment['SOURCE_DATE_EPOCH'] = '946684800'
    $info.Environment['TEMP'] = $Temp
    $info.Environment['TMP'] = $Temp

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    [void]$process.Start()
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    $process.WaitForExit()
    $outText = $stdout.GetAwaiter().GetResult()
    $errorText = $stderr.GetAwaiter().GetResult()
    if ($process.ExitCode -ne 0) {
        throw "$Label failed with exit code $($process.ExitCode)`n$outText`n$errorText"
    }
    [ordered]@{
        label = $Label
        remap_destinations = @('/workspace', '/target', '/cargo', '/user', '/home', '/tmp')
    }
}

$expectedRustcVersion = '1.97.1'
$expectedRustcCommit = '8bab26f4f68e0e26f0bb7960be334d5b520ea452'
function Select-PinnedRustToolchain {
    $rustup = Get-Command rustup -CommandType Application -ErrorAction SilentlyContinue
    if ($null -eq $rustup) { throw 'Rust 1.97.1 is required. Install it with: rustup toolchain install 1.97.1-x86_64-pc-windows-msvc --profile minimal' }
    $script:rustupPath = $rustup.Source
    foreach ($line in @(& $script:rustupPath toolchain list)) {
        $name = ($line -split '\s+')[0]
        if ([string]::IsNullOrWhiteSpace($name)) { continue }
        $verbose = (& $script:rustupPath run $name rustc --version --verbose | Out-String).Replace("`r", '')
        if ($LASTEXITCODE -eq 0 -and $verbose -match "(?m)^release: $([regex]::Escape($expectedRustcVersion))$" -and
            $verbose -match "(?m)^commit-hash: $expectedRustcCommit$" -and
            $verbose -match '(?m)^host: x86_64-pc-windows-msvc$') { return $name }
    }
    throw 'Rust 1.97.1 (8bab26f4f68e0e26f0bb7960be334d5b520ea452) for x86_64-pc-windows-msvc is required. Install it with: rustup toolchain install 1.97.1-x86_64-pc-windows-msvc --profile minimal'
}

$source = Assert-LocalDirectory -Path $SourceRoot -Label 'SourceRoot'
$output = Assert-LocalDirectory -Path $OutputDir -Label 'OutputDir'
if (Test-PathOverlap $source $output) { throw 'SourceRoot and OutputDir must not overlap' }
if (@(Get-ChildItem -LiteralPath $output -Force).Count -ne 0) { throw 'OutputDir must be empty' }

$rustupToolchainBefore = [Environment]::GetEnvironmentVariable('RUSTUP_TOOLCHAIN', 'Process')
$selectedToolchain = Select-PinnedRustToolchain
$env:RUSTUP_TOOLCHAIN = $selectedToolchain
$cargoPath = (& $rustupPath which --toolchain $selectedToolchain cargo).Trim()
if ($LASTEXITCODE -ne 0) { throw 'cannot resolve cargo from the pinned installed toolchain' }
$rustcPath = (& $rustupPath which --toolchain $selectedToolchain rustc).Trim()
if ($LASTEXITCODE -ne 0) { throw 'cannot resolve rustc from the pinned installed toolchain' }
$rustcVerbose = (& $rustcPath --version --verbose | Out-String).Replace("`r", '')
$rustcVersion = (& $rustcPath --version).Trim()
$cargoVersion = (& $cargoPath --version).Trim()
$cargoHome = if ($env:CARGO_HOME) { [IO.Path]::GetFullPath($env:CARGO_HOME) } else { Join-Path $env:USERPROFILE '.cargo' }
$work = Join-Path ([IO.Path]::GetTempPath()) ('lcdsirplus-repro-' + [Guid]::NewGuid().ToString('N'))
$detached = Join-Path $work 'detached-source'
$targetSource = Join-Path $work 'target-source'
$targetDetached = Join-Path $work 'target-detached'
$tempSource = Join-Path $work 'temp-source'
$tempDetached = Join-Path $work 'temp-detached'

try {
    [IO.Directory]::CreateDirectory($detached) | Out-Null
    $sourceFiles = @(Get-SourceFiles $source)
    $sourceManifest = Get-SourceManifest $sourceFiles
    foreach ($file in $sourceFiles) {
        $destination = Join-Path $detached $file.Relative.Replace('/', '\')
        [IO.Directory]::CreateDirectory((Split-Path $destination -Parent)) | Out-Null
        [IO.File]::Copy($file.FullName, $destination, $false)
    }
    $detachedFiles = @(Get-SourceFiles $detached)
    $detachedManifest = Get-SourceManifest $detachedFiles
    Assert-ManifestsEqual $sourceManifest $detachedManifest 'detached source copy is not byte-identical'

    $sourceBuild = Invoke-ReproducibleBuild -Root $source -Target $targetSource -Temp $tempSource -Label 'source'
    $detachedBuild = Invoke-ReproducibleBuild -Root $detached -Target $targetDetached -Temp $tempDetached -Label 'detached'

    $sourceExe = Join-Path $targetSource 'release\LCDSirPlus.exe'
    $detachedExe = Join-Path $targetDetached 'release\LCDSirPlus.exe'
    $sourceConfig = Join-Path $targetSource 'release\lcdsirplus.txt'
    $detachedConfig = Join-Path $targetDetached 'release\lcdsirplus.txt'
    $sourceExeIdentity = Get-FileIdentity $sourceExe
    $detachedExeIdentity = Get-FileIdentity $detachedExe
    $sourceConfigIdentity = Get-FileIdentity $sourceConfig
    $detachedConfigIdentity = Get-FileIdentity $detachedConfig
    Assert-FilesEqual $sourceExe $detachedExe 'release executables are not byte-identical'
    Assert-FilesEqual $sourceConfig $detachedConfig 'release configurations are not byte-identical'

    $sourceRc = @(Get-ChildItem -LiteralPath (Join-Path $targetSource 'release\build') -Filter resource.rc -File -Recurse)
    $detachedRc = @(Get-ChildItem -LiteralPath (Join-Path $targetDetached 'release\build') -Filter resource.rc -File -Recurse)
    $sourceLib = @(Get-ChildItem -LiteralPath (Join-Path $targetSource 'release\build') -Filter resource.lib -File -Recurse)
    $detachedLib = @(Get-ChildItem -LiteralPath (Join-Path $targetDetached 'release\build') -Filter resource.lib -File -Recurse)
    if ($sourceRc.Count -ne 1 -or $detachedRc.Count -ne 1 -or $sourceLib.Count -ne 1 -or $detachedLib.Count -ne 1) {
        throw 'expected exactly one generated resource.rc and resource.lib per build'
    }
    $sourceRcIdentity = Get-FileIdentity $sourceRc[0].FullName
    $detachedRcIdentity = Get-FileIdentity $detachedRc[0].FullName
    $sourceLibIdentity = Get-FileIdentity $sourceLib[0].FullName
    $detachedLibIdentity = Get-FileIdentity $detachedLib[0].FullName
    Assert-FilesEqual $sourceRc[0].FullName $detachedRc[0].FullName 'generated resource.rc files are not byte-identical'
    Assert-FilesEqual $sourceLib[0].FullName $detachedLib[0].FullName 'generated resource.lib files are not byte-identical'
    $rcText = [IO.File]::ReadAllText($sourceRc[0].FullName, [Text.Encoding]::UTF8)
    if ($rcText -notmatch '(?m)^1 ICON "LCDSirPlus\.ico"$' -or $rcText -match '(?im)^\s*\d+\s+24(?:\s|$)|manifest') {
        throw 'generated resource icon or manifest contract changed'
    }

    Assert-ManifestsEqual $sourceManifest (Get-SourceManifest @(Get-SourceFiles $source)) 'SourceRoot changed during builds'
    Assert-ManifestsEqual $detachedManifest (Get-SourceManifest @(Get-SourceFiles $detached)) 'detached source changed during builds'
    $versionSource = [Diagnostics.FileVersionInfo]::GetVersionInfo($sourceExe)
    $versionDetached = [Diagnostics.FileVersionInfo]::GetVersionInfo($detachedExe)
    foreach ($property in @('FileDescription', 'FileVersion', 'ProductName', 'ProductVersion')) {
        if ($versionSource.$property -cne $versionDetached.$property) { throw "VERSIONINFO mismatch: $property" }
    }

    [IO.File]::Copy($sourceExe, (Join-Path $output 'LCDSirPlus.exe'), $false)
    [IO.File]::Copy($sourceConfig, (Join-Path $output 'lcdsirplus.txt'), $false)
    $evidence = [ordered]@{
        schema_version = 2
        source = [ordered]@{
            root = 'source-root'
            commit = $SourceCommit
            file_count = $sourceManifest.Count
            manifest_sha256 = $sourceManifest.Sha256
            exclusions = @('.git/**', '.codex/**', 'artifacts/**', 'target/**', 'lcdsirplus.local.txt')
            detached_copy_equal = $true
            source_unchanged = $true
        }
        tools = [ordered]@{
            cargo = [ordered]@{ label = 'pinned-toolchain/cargo'; sha256 = (Get-FileHash $cargoPath -Algorithm SHA256).Hash.ToLowerInvariant(); version = $cargoVersion }
            rustc = [ordered]@{
                label = 'pinned-toolchain/rustc'
                sha256 = (Get-FileHash $rustcPath -Algorithm SHA256).Hash.ToLowerInvariant()
                version = $rustcVersion
                commit = ([regex]::Match($rustcVerbose, '(?m)^commit-hash: ([0-9a-f]{40})$')).Groups[1].Value
                host = 'x86_64-pc-windows-msvc'
            }
            powershell = $PSVersionTable.PSVersion.ToString()
            command = 'cargo build --release --locked --offline'
            controlled_environment = $true
            brepro = $true
        }
        builds = @($sourceBuild, $detachedBuild)
        outputs = [ordered]@{
            executable = [ordered]@{ source = $sourceExeIdentity; detached = $detachedExeIdentity; full_bytes_equal = $true }
            config = [ordered]@{ source = $sourceConfigIdentity; detached = $detachedConfigIdentity; full_bytes_equal = $true }
        }
        resources = [ordered]@{
            inspection_mode = 'full executable equality plus generated resource equality and FileVersionInfo'
            resource_rc = [ordered]@{ source = $sourceRcIdentity; detached = $detachedRcIdentity; full_bytes_equal = $true }
            resource_lib = [ordered]@{ source = $sourceLibIdentity; detached = $detachedLibIdentity; full_bytes_equal = $true }
            version_info = [ordered]@{
                FileDescription = $versionSource.FileDescription
                FileVersion = $versionSource.FileVersion
                ProductName = $versionSource.ProductName
                ProductVersion = $versionSource.ProductVersion
                exact_equal = $true
            }
            icon_group_manifest_state_covered_by_full_executable_equality = $true
            generated_rc_icon_id = 1
            expected_manifest_present = $false
        }
        result = 'PASS'
    }
    $json = ($evidence | ConvertTo-Json -Depth 8 -EscapeHandling EscapeNonAscii) + "`n"
    [IO.File]::WriteAllText((Join-Path $output 'REPRODUCIBILITY.json'), $json, [Text.Encoding]::ASCII)
}
finally {
    if ([IO.Directory]::Exists($work)) { Remove-Item -LiteralPath $work -Recurse -Force }
    if ($null -eq $rustupToolchainBefore) { Remove-Item Env:\RUSTUP_TOOLCHAIN -ErrorAction SilentlyContinue }
    else { $env:RUSTUP_TOOLCHAIN = $rustupToolchainBefore }
}

Write-Host "Reproducible release build verified: $output" -ForegroundColor Green
