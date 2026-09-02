<#
.SYNOPSIS
Runs the repository build, test, documentation, and smoke-test gates.

.DESCRIPTION
Checks PowerShell, Rust, documentation, and product behavior with the pinned toolchain. Commands update Rust build outputs under target/; temporary fixtures are removed. The default run also creates the release executable and configuration in target/release and runs Windows CLI and GUI smoke tests, but not physical hardware acceptance.

.PARAMETER ReleaseOnly
Stops after formatting, lint, and release-mode test gates, skipping the final application build and Windows CLI and GUI smoke tests.

.EXAMPLE
PS> .\scripts\Test.ps1
Runs all automated repository gates and smoke tests.
#>
#Requires -Version 7

[CmdletBinding()]
param(
    [switch]$ReleaseOnly
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
$config = Join-Path $repo 'lcdsirplus.txt'
Set-Location -LiteralPath $repo

function Invoke-LcdCli {
    param([string]$Executable, [string[]]$Arguments)
    $output = (& $Executable @Arguments 2>&1 | Out-String)
    [pscustomobject]@{ Output = $output; ExitCode = $LASTEXITCODE }
}

function Get-PeSubsystem {
    param([string]$Path)
    $stream = [IO.File]::OpenRead($Path)
    try {
        $reader = [IO.BinaryReader]::new($stream)
        try {
            $stream.Position = 0x3c
            $peOffset = $reader.ReadUInt32()
            $stream.Position = $peOffset
            if ($reader.ReadUInt32() -ne 0x00004550) { throw 'release executable has no PE signature' }
            $stream.Position = $peOffset + 4 + 20 + 68
            return $reader.ReadUInt16()
        }
        finally { $reader.Dispose() }
    }
    finally { $stream.Dispose() }
}

function Wait-NativeWindow {
    param([Diagnostics.Process]$Process, [string]$ClassName)
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        $Process.Refresh()
        if ($Process.HasExited) { return [IntPtr]::Zero }
        $window = [LCDSirPlus.NativeWindow]::Find($Process.Id, $ClassName)
        if ($window -ne [IntPtr]::Zero) { return $window }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    return [IntPtr]::Zero
}

function Stop-TestProcess {
    param([Diagnostics.Process]$Process, [IntPtr]$Window)
    if ($null -eq $Process) { return }
    $Process.Refresh()
    if ($Process.HasExited) { return }
    if ($Window -ne [IntPtr]::Zero) {
        [void][LCDSirPlus.NativeWindow]::PostMessage($Window, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
        if ($Process.WaitForExit(10000)) { return }
    }
    $Process.Kill($true)
    [void]$Process.WaitForExit(10000)
}

$expectedRustcVersion = '1.97.1'
$expectedRustcCommit = '8bab26f4f68e0e26f0bb7960be334d5b520ea452'
function Select-PinnedRustToolchain {
    $rustup = Get-Command rustup -CommandType Application -ErrorAction SilentlyContinue
    $install = 'rustup toolchain install 1.97.1 --profile minimal --component clippy --component rustfmt --target x86_64-pc-windows-msvc'
    if ($null -eq $rustup) { throw "Rust 1.97.1 is required. Install it with: $install" }
    $name = [Environment]::GetEnvironmentVariable('RUSTUP_TOOLCHAIN', 'Process')
    if ([string]::IsNullOrWhiteSpace($name)) {
        $name = '1.97.1-x86_64-pc-windows-msvc'
        $installed = @(& $rustup.Source toolchain list | ForEach-Object { ($_ -split '\s+')[0] })
        if ($name -cnotin $installed) { throw "The exact Rust 1.97.1 toolchain alias is not installed. Install it with: $install" }
    }
    $verbose = (& $rustup.Source run $name rustc --version --verbose | Out-String).Replace("`r", '')
    $components = @(& $rustup.Source component list --toolchain $name --installed)
    $targets = @(& $rustup.Source target list --toolchain $name --installed)
    if ($LASTEXITCODE -ne 0 -or $verbose -notmatch "(?m)^release: $([regex]::Escape($expectedRustcVersion))$" -or
        $verbose -notmatch "(?m)^commit-hash: $expectedRustcCommit$" -or
        $verbose -notmatch '(?m)^host: x86_64-pc-windows-msvc$' -or
        @($components | Where-Object { $_ -match '^clippy-' }).Count -ne 1 -or
        @($components | Where-Object { $_ -match '^rustfmt-' }).Count -ne 1 -or
        @($targets | Where-Object { $_ -ceq 'x86_64-pc-windows-msvc' }).Count -ne 1) {
        throw "Selected Rust toolchain '$name' is not the pinned 1.97.1 provenance with required components and target. Install it with: $install"
    }
    return $name
}

$toolchain = [IO.File]::ReadAllText((Join-Path $repo 'rust-toolchain.toml'), [Text.Encoding]::UTF8).Replace("`r`n", "`n")
if ($toolchain -cne "[toolchain]`nchannel = `"1.97.1`"`nprofile = `"minimal`"`ncomponents = [`"clippy`", `"rustfmt`"]`ntargets = [`"x86_64-pc-windows-msvc`"]`n") {
    throw 'rust-toolchain.toml does not pin the exact Rust version, minimal profile, components, and Windows MSVC target'
}
$env:RUSTUP_TOOLCHAIN = Select-PinnedRustToolchain
$noticeText = [IO.File]::ReadAllText((Join-Path $repo 'THIRD_PARTY_LICENSES.txt'), [Text.Encoding]::UTF8)
if ($noticeText.IndexOf('rustc 1.97.1', [StringComparison]::Ordinal) -lt 0 -or
    $noticeText.IndexOf($expectedRustcCommit, [StringComparison]::Ordinal) -lt 0) {
    throw 'third-party notices do not identify the pinned rustc version and commit'
}

Write-Host '== PowerShell syntax ==' -ForegroundColor Cyan
$parse = '$tokens=$null; $errors=$null; [Management.Automation.Language.Parser]::ParseFile($env:LCDSIRPLUS_PARSE_FILE,[ref]$tokens,[ref]$errors) | Out-Null; if ($errors.Count -ne 0) { $errors | ForEach-Object { Write-Error $_ }; exit 1 }'
$allScripts = @(Get-ChildItem -LiteralPath $PSScriptRoot -Filter '*.ps1' -File | ForEach-Object { $_.Name } | Sort-Object)
foreach ($script in $allScripts) {
    $env:LCDSIRPLUS_PARSE_FILE = Join-Path $PSScriptRoot $script
    & (Get-Command pwsh).Source -NoProfile -Command $parse
    if ($LASTEXITCODE -ne 0) { throw "pwsh syntax failed: $script" }
}
foreach ($script in @('Acquire-InnoSetup.ps1')) {
    $env:LCDSIRPLUS_PARSE_FILE = Join-Path $PSScriptRoot $script
    & (Get-Command powershell.exe).Source -NoProfile -Command $parse
    if ($LASTEXITCODE -ne 0) { throw "Windows PowerShell 5.1 syntax failed: $script" }
}
Remove-Item Env:\LCDSIRPLUS_PARSE_FILE -ErrorAction SilentlyContinue

Write-Host '== vendored winres tests ==' -ForegroundColor Cyan
$vendorTestRoot = Join-Path ([IO.Path]::GetTempPath()) ('lcdsirplus-winres-' + [Guid]::NewGuid().ToString('N'))
$cargoTargetBefore = [Environment]::GetEnvironmentVariable('CARGO_TARGET_DIR', 'Process')
try {
    [IO.Directory]::CreateDirectory($vendorTestRoot) | Out-Null
    Copy-Item -LiteralPath (Join-Path $repo 'third_party\winres') -Destination $vendorTestRoot -Recurse
    $env:CARGO_TARGET_DIR = Join-Path $vendorTestRoot 'target'
    cargo test --manifest-path (Join-Path $vendorTestRoot 'winres\Cargo.toml') --lib --offline
    if ($LASTEXITCODE -ne 0) { throw 'vendored winres tests failed' }
}
finally {
    if ($null -eq $cargoTargetBefore) { Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
    else { $env:CARGO_TARGET_DIR = $cargoTargetBefore }
    if ([IO.Directory]::Exists($vendorTestRoot)) { Remove-Item -LiteralPath $vendorTestRoot -Recurse -Force }
}

Write-Host '== product identity ==' -ForegroundColor Cyan
$retiredPattern = 'lcd' + '([-_ ]?)' + 'for' + 'ge2?'
$trackedNames = @(git ls-files --cached --others --exclude-standard -- . ':(exclude)docs/OldDocs.7z' | Where-Object {
        $_ -cne 'docs/OldDocs.7z' -and (Test-Path -LiteralPath $_)
    })
if ($LASTEXITCODE -ne 0 -or @($trackedNames | Where-Object { $_ -match $retiredPattern }).Count -ne 0) { throw 'tracked filename contains the retired product identity' }
foreach ($name in $trackedNames) {
    if ([IO.File]::ReadAllText((Join-Path $repo $name)).ToLowerInvariant() -match $retiredPattern) { throw "tracked content contains the retired product identity: $name" }
}

Write-Host '== documentation ==' -ForegroundColor Cyan
$configSource = [IO.File]::ReadAllText((Join-Path $repo 'src\config.rs'))
$moduleBlock = [regex]::Match($configSource, 'pub const MODULES:\s*&\[&str\]\s*=\s*&\[(.*?)\];', [Text.RegularExpressions.RegexOptions]::Singleline)
if (-not $moduleBlock.Success) { throw 'canonical module registry not found' }
$canonicalModules = @([regex]::Matches($moduleBlock.Groups[1].Value, '"([A-Z][A-Z0-9_]*)"') | ForEach-Object { $_.Groups[1].Value })
$modules = [IO.File]::ReadAllText((Join-Path $repo 'modules.md'))
$manual = [IO.File]::ReadAllText((Join-Path $repo 'docs\INSTRUCTION-MANUAL.md'))
$tableHeaderPattern = '(?m)^\| Option \| Description \|\r?$'
$tableRowPattern = '(?m)^(\| `([A-Z][A-Z0-9_]*)` \| .+ \|)\r?$'
if ([regex]::Matches($modules, $tableHeaderPattern).Count -ne 1 -or
    [regex]::Matches($manual, $tableHeaderPattern).Count -ne 1) {
    throw 'modules and instruction manual must each contain exactly one button option table'
}
$moduleRows = @([regex]::Matches($modules, $tableRowPattern) | ForEach-Object { $_.Groups[1].Value })
$manualRows = @([regex]::Matches($manual, $tableRowPattern) | ForEach-Object { $_.Groups[1].Value })
$documentedModules = @([regex]::Matches($modules, $tableRowPattern) | ForEach-Object { $_.Groups[2].Value })
if ($canonicalModules.Count -ne 53 -or $moduleRows.Count -ne 53 -or $manualRows.Count -ne 53) {
    throw "button option inventory mismatch: source=$($canonicalModules.Count), modules=$($moduleRows.Count), manual=$($manualRows.Count)"
}
for ($index = 0; $index -lt $canonicalModules.Count; $index++) {
    if ($canonicalModules[$index] -cne $documentedModules[$index]) { throw "module reference order mismatch at index $index" }
    if ($moduleRows[$index] -cne $manualRows[$index]) { throw "button option table row mismatch at index $index" }
}
$readme = [IO.File]::ReadAllText((Join-Path $repo 'README.md'))
if ($readme -notmatch '\]\(modules\.md\)' -or $manual -notmatch '\]\(\.\./modules\.md\)') { throw 'quick-reference links are missing' }
$parserSource = [IO.File]::ReadAllText((Join-Path $repo 'src\parser.rs'))
$applyBlock = [regex]::Match($parserSource, 'fn apply\(.*?match key \{(.*?)other =>', [Text.RegularExpressions.RegexOptions]::Singleline)
if (-not $applyBlock.Success) { throw 'configuration parser key registry not found' }
$acceptedConfigKeys = @('include') + @([regex]::Matches($applyBlock.Groups[1].Value, '"([a-z][a-z0-9_]*)"\s*(?:\||=>)') | ForEach-Object { $_.Groups[1].Value })
$configuration = [IO.File]::ReadAllText((Join-Path $repo 'docs\CONFIGURATION.md'))
$documentedConfigKeys = @([regex]::Matches($configuration, '(?m)^\| `([a-z][a-z0-9_]*)` \|') | ForEach-Object { $_.Groups[1].Value })
$missingConfigKeys = @($acceptedConfigKeys | Where-Object { $_ -cnotin $documentedConfigKeys })
$extraConfigKeys = @($documentedConfigKeys | Where-Object { $_ -cnotin $acceptedConfigKeys })
if ($acceptedConfigKeys.Count -ne $documentedConfigKeys.Count -or $missingConfigKeys.Count -ne 0 -or $extraConfigKeys.Count -ne 0) {
    throw "configuration reference key mismatch: parser=$($acceptedConfigKeys.Count), docs=$($documentedConfigKeys.Count), missing=$($missingConfigKeys -join ','), extra=$($extraConfigKeys -join ',')"
}
foreach ($key in $acceptedConfigKeys) {
    if (@($documentedConfigKeys | Where-Object { $_ -ceq $key }).Count -ne 1) { throw "configuration reference entry mismatch: $key" }
}
$compatibilityConfigKeys = @('date_format', 'time_format', 'headset_estimate_hours', 'discord_redirect_uri', 'hang_button')
$compatibilitySection = [regex]::Match($configuration, '(?ms)^## Advanced compatibility-only settings\s+(.*?)(?=^## |\z)')
if (-not $compatibilitySection.Success) { throw 'advanced compatibility-only configuration section is missing' }
$documentedCompatibilityKeys = @([regex]::Matches($compatibilitySection.Groups[1].Value, '(?m)^\| `([a-z][a-z0-9_]*)` \|') | ForEach-Object { $_.Groups[1].Value })
if ($documentedCompatibilityKeys.Count -ne $compatibilityConfigKeys.Count) { throw 'advanced compatibility-only configuration section contains an active or missing key' }
foreach ($key in $compatibilityConfigKeys) {
    if (@([regex]::Matches($compatibilitySection.Groups[1].Value, "(?m)^\| ``$key`` \|")).Count -ne 1) { throw "compatibility-only configuration key is not isolated: $key" }
}
$template = [IO.File]::ReadAllText($config)
$templateKeys = @([regex]::Matches($template, '(?m)^([a-z][a-z0-9_]*)\s+') | ForEach-Object { $_.Groups[1].Value })
foreach ($key in $compatibilityConfigKeys) {
    if ($key -cin $templateKeys) { throw "compatibility-only key appears in default template: $key" }
}
if (@($templateKeys | Where-Object { $_ -cnotin $acceptedConfigKeys }).Count -ne 0 -or
    @($templateKeys | Group-Object | Where-Object Count -ne 1).Count -ne 0) {
    throw 'default template contains an unknown or duplicate active key'
}
$omittedSensorConfigKeys = @(
    'lhm_cpu_temp_sensor', 'lhm_gpu_temp_sensor', 'lhm_vrm_temp_sensor', 'lhm_chipset_temp_sensor',
    'lhm_motherboard_temp_sensor', 'lhm_cpu_fan_control_sensor', 'lhm_cpu_fan_rpm_sensor',
    'lhm_pump_control_sensor', 'lhm_pump_rpm_sensor', 'lhm_total_power_sensor', 'lhm_cpu_power_sensor',
    'lhm_gpu_power_sensor', 'hwinfo_cpu_temp_sensor', 'hwinfo_cpu_temp_reading',
    'hwinfo_total_power_sensor', 'hwinfo_total_power_reading', 'hwinfo_cpu_power_sensor',
    'hwinfo_cpu_power_reading', 'hwinfo_gpu_power_sensor', 'hwinfo_gpu_power_reading'
)
$expectedTemplateKeys = @($acceptedConfigKeys | Where-Object {
        $_ -cne 'include' -and $_ -cnotin $compatibilityConfigKeys -and $_ -cnotin $omittedSensorConfigKeys
    })
if ($templateKeys.Count -ne $expectedTemplateKeys.Count -or
    @($expectedTemplateKeys | Where-Object { $_ -cnotin $templateKeys }).Count -ne 0) {
    throw 'default template does not contain every shipped active default key'
}
foreach ($line in @(
    'safe_mode               0',
    'ccd_source              auto',
    'ccd_cache_processors    auto',
    'ccd_frequency_processors auto',
    'presentmon_enabled      1',
    'presentmon_target_mode  presenting',
    'presentmon_deferred     1',
    'presentmon_persist      0',
    'presentmon_process_name ""',
    'presentmon_exclude      dwm.exe explorer.exe applicationframehost.exe textinputhost.exe searchhost.exe lcdsirplus.exe lcdsirplus.console.exe'
)) {
    if (-not $template.Contains($line, [StringComparison]::Ordinal)) { throw "default template active default mismatch: $line" }
}
$publicDocuments = @(
    'README.md', 'modules.md', 'RELEASE-NOTES.md', 'SECURITY.md',
    'docs\CONFIGURATION.md', 'docs\INSTRUCTION-MANUAL.md'
)
foreach ($document in $publicDocuments) {
    if ([IO.File]::ReadAllText((Join-Path $repo $document)).Contains('HARDWARE-ACCEPTANCE.md', [StringComparison]::OrdinalIgnoreCase)) {
        throw "public document links the source-only hardware acceptance ledger: $document"
    }
}
foreach ($script in @('scripts\Build.ps1', 'scripts\Package-Test.ps1')) {
    $scriptText = [IO.File]::ReadAllText((Join-Path $repo $script)).Replace('\', '/')
    if ($scriptText.Contains('docs/HARDWARE-ACCEPTANCE.md', [StringComparison]::OrdinalIgnoreCase)) {
        throw "portable inventory includes the source-only hardware acceptance ledger: $script"
    }
}
$buildInventory = [IO.File]::ReadAllText((Join-Path $repo 'scripts\Build.ps1'))
$installerInventory = [IO.File]::ReadAllText((Join-Path $repo 'packaging\LCDSirPlus.iss'))
$packageInventory = [IO.File]::ReadAllText((Join-Path $repo 'scripts\Package-Test.ps1'))
foreach ($document in @('RELEASE-NOTES.md', 'SECURITY.md')) {
    if (@([regex]::Matches($buildInventory, "'$([regex]::Escape($document))'")).Count -ne 2 -or
        @([regex]::Matches($installerInventory, "(?m)^Source: .*\\$([regex]::Escape($document))`"; DestDir:")).Count -ne 1 -or
        @([regex]::Matches($packageInventory, "'$([regex]::Escape($document))'")).Count -lt 2) {
        throw "installed document inventory is not synchronized: $document"
    }
}
foreach ($shortcut in @(
    'Name: "{autoprograms}\LCDSirPlus\Instruction Manual"; Filename: "{app}\docs\LCDSirPlus-Instruction-Manual.pdf"; Tasks: startmenu',
    'Name: "{autoprograms}\LCDSirPlus\Security"; Filename: "{app}\SECURITY.md"; Tasks: startmenu'
)) {
    if ($installerInventory.IndexOf($shortcut, [StringComparison]::Ordinal) -lt 0 -or
        $packageInventory.IndexOf("'$shortcut'", [StringComparison]::Ordinal) -lt 0) {
        throw "installed document shortcut is not synchronized: $shortcut"
    }
}

Write-Host '== cargo fmt check ==' -ForegroundColor Cyan
cargo fmt --check
if ($LASTEXITCODE -ne 0) { throw 'cargo fmt check failed' }

Write-Host '== cargo clippy (zero-warning policy) ==' -ForegroundColor Cyan
cargo clippy --all-targets 2>&1 | Tee-Object -Variable clippyOutput
if ($LASTEXITCODE -ne 0) { throw 'cargo clippy failed' }
$warnings = @($clippyOutput | Where-Object { $_ -match 'warning:' })
if ($warnings.Count -gt 0) { throw "clippy emitted $($warnings.Count) warnings; zero-warning policy violated" }

Write-Host '== cargo test ==' -ForegroundColor Cyan
cargo test --release --locked
if ($LASTEXITCODE -ne 0) { throw 'cargo test failed' }

if ($ReleaseOnly) { return }

Write-Host '== release build ==' -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'release build failed' }

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

namespace LCDSirPlus {
    public static class NativeWindow {
        private delegate bool EnumWindowsProc(IntPtr window, IntPtr parameter);
        [DllImport("user32.dll")] private static extern bool EnumWindows(EnumWindowsProc callback, IntPtr parameter);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] private static extern int GetClassName(IntPtr window, StringBuilder name, int count);
        [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] private static extern int GetWindowText(IntPtr window, StringBuilder text, int count);
        [DllImport("kernel32.dll")] private static extern uint GetConsoleProcessList([Out] uint[] processList, uint count);
        [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr window, uint message, IntPtr wParam, IntPtr lParam);

        public static IntPtr Find(int processId, string className) {
            IntPtr found = IntPtr.Zero;
            EnumWindows(delegate(IntPtr window, IntPtr parameter) {
                uint owner;
                GetWindowThreadProcessId(window, out owner);
                var name = new StringBuilder(64);
                GetClassName(window, name, name.Capacity);
                if (owner == processId && name.ToString() == className) {
                    found = window;
                    return false;
                }
                return true;
            }, IntPtr.Zero);
            return found;
        }

        public static string Text(IntPtr window) {
            var text = new StringBuilder(256);
            GetWindowText(window, text, text.Capacity);
            return text.ToString();
        }

        public static uint[] ConsoleProcesses() {
            var processes = new uint[16];
            uint count = GetConsoleProcessList(processes, (uint)processes.Length);
            if (count > (uint)processes.Length) {
                processes = new uint[count];
                count = GetConsoleProcessList(processes, count);
            }
            Array.Resize(ref processes, (int)Math.Min(count, (uint)processes.Length));
            return processes;
        }
    }
}
'@

Write-Host '== smoke: --version ==' -ForegroundColor Cyan
$exe = Join-Path $repo 'target\release\LCDSirPlus.exe'
$version = Invoke-LcdCli $exe @('--version')
if ($version.ExitCode -ne 0 -or $version.Output.Trim() -cne 'LCDSirPlus 0.3.0') { throw '--version identity failed' }

Write-Host '== smoke: --help identity ==' -ForegroundColor Cyan
$help = Invoke-LcdCli $exe @('--help')
if ($help.ExitCode -ne 0 -or $help.Output -notmatch 'LCDSirPlus\.exe' -or $help.Output -notmatch '--hardware-discover' -or $help.Output -notmatch '--help, -h' -or $help.Output -notmatch '--version, -v' -or $help.Output -match $retiredPattern) { throw '--help identity failed' }

Write-Host '== smoke: Windows GUI subsystem ==' -ForegroundColor Cyan
if ((Get-PeSubsystem $exe) -ne 2) { throw 'release executable is not IMAGE_SUBSYSTEM_WINDOWS_GUI' }

Write-Host '== smoke: --validate-config ==' -ForegroundColor Cyan
$builtConfig = Join-Path $repo 'target\release\lcdsirplus.txt'
if (-not [IO.File]::Exists($builtConfig) -or
    [Convert]::ToBase64String([IO.File]::ReadAllBytes($builtConfig)) -cne [Convert]::ToBase64String([IO.File]::ReadAllBytes($config)) -or
    [IO.File]::Exists((Join-Path $repo 'target\release\lcdsirplus.local.txt'))) {
    throw 'release build configuration copy is invalid'
}
$validation = Invoke-LcdCli $exe @('--validate-config')
if ($validation.ExitCode -ne 0 -or $validation.Output -notmatch '^OK:') { throw "built configuration is invalid`n$($validation.Output)" }

$invalidConfig = Join-Path ([IO.Path]::GetTempPath()) ('LCDSirPlus-invalid-' + [Guid]::NewGuid().ToString('N') + '.txt')
try {
    [IO.File]::WriteAllText($invalidConfig, "unknown_setting 1`n")
    $invalid = Invoke-LcdCli $exe @('--validate-config', '--config', $invalidConfig)
    if ($invalid.ExitCode -ne 2 -or $invalid.Output -notmatch 'INVALID:') { throw 'invalid configuration CLI diagnostics/exit failed' }
}
finally { [IO.File]::Delete($invalidConfig) }

$orphanBackend = Invoke-LcdCli $exe @('--backend', 'virtual')
if ($orphanBackend.ExitCode -ne 2 -or $orphanBackend.Output -notmatch '--backend requires --hardware-test') { throw 'orphan --backend validation failed' }
$orphanDuration = Invoke-LcdCli $exe @('--duration-secs', '1')
if ($orphanDuration.ExitCode -ne 2 -or $orphanDuration.Output -notmatch '--duration-secs requires --hardware-test') { throw 'orphan --duration-secs validation failed' }

Write-Host '== smoke: --hardware-discover (read-only) ==' -ForegroundColor Cyan
$discover = Invoke-LcdCli $exe @('--hardware-discover')
$discover.Output | Write-Host
Write-Host '   (exit code 3 = no G13 present; acceptable on machines without the device)'

Write-Host '== smoke: --hardware-test --backend virtual ==' -ForegroundColor Cyan
$hardware = Invoke-LcdCli $exe @('--hardware-test', '--backend', 'virtual', '--duration-secs', '1', '--config', $config)
$hardware.Output | Write-Host
if ($hardware.ExitCode -ne 0) { throw 'virtual hardware test failed' }

Write-Host '== smoke: console-free no-argument lifecycle ==' -ForegroundColor Cyan
$instance = Invoke-LcdCli $exe @('--instance-smoke-child')
if ($instance.ExitCode -eq 0) { throw 'normal lifecycle smoke requires no existing LCDSirPlus instance; no process was stopped' }
$lifecycleRoot = Join-Path ([IO.Path]::GetTempPath()) ('LCDSirPlus-lifecycle-' + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($lifecycleRoot) | Out-Null
$lifecycleExe = Join-Path $lifecycleRoot 'LCDSirPlus.exe'
Copy-Item -LiteralPath $exe -Destination $lifecycleExe
$testConfig = [IO.File]::ReadAllText($builtConfig)
$testConfig = [Text.RegularExpressions.Regex]::Replace($testConfig, '(?m)^preview_mode\s+\S+', 'preview_mode            never')
$testConfig = [Text.RegularExpressions.Regex]::Replace($testConfig, '(?m)^start_minimized\s+\S+', 'start_minimized         1')
$testConfig = [Text.RegularExpressions.Regex]::Replace($testConfig, '(?m)^safe_mode\s+\S+', 'safe_mode               1')
[IO.File]::WriteAllText((Join-Path $lifecycleRoot 'lcdsirplus.txt'), $testConfig)
$process = $null
$window = [IntPtr]::Zero
try {
    $process = Start-Process -FilePath $lifecycleExe -WorkingDirectory $lifecycleRoot -PassThru
    $window = Wait-NativeWindow $process 'LCDSIRPLUS_PREVIEW'
    if ($window -eq [IntPtr]::Zero) { throw 'no-argument launch did not remain alive with its tray window' }
    if ([LCDSirPlus.NativeWindow]::ConsoleProcesses() -contains [uint32]$process.Id) { throw 'no-argument launch attached to the parent console' }
    if ([LCDSirPlus.NativeWindow]::Find($process.Id, 'ConsoleWindowClass') -ne [IntPtr]::Zero) { throw 'no-argument launch owns a console window' }
    Start-Sleep -Milliseconds 500
    $process.Refresh()
    if ($process.HasExited) { throw 'no-argument tray runtime exited unexpectedly' }
    Stop-TestProcess $process $window
    if ($process.ExitCode -ne 0) { throw "graceful tray shutdown returned $($process.ExitCode)" }
}
finally {
    Stop-TestProcess $process $window
    [IO.Directory]::Delete($lifecycleRoot, $true)
}

Write-Host '== smoke: GUI startup error dialog ==' -ForegroundColor Cyan
$errorRoot = Join-Path ([IO.Path]::GetTempPath()) ('LCDSirPlus-error-' + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($errorRoot) | Out-Null
$errorExe = Join-Path $errorRoot 'LCDSirPlus.exe'
Copy-Item -LiteralPath $exe -Destination $errorExe
[IO.File]::WriteAllText((Join-Path $errorRoot 'lcdsirplus.txt'), "unknown_setting 1`n")
$process = $null
$dialog = [IntPtr]::Zero
try {
    $process = Start-Process -FilePath $errorExe -WorkingDirectory $errorRoot -PassThru
    $dialog = Wait-NativeWindow $process '#32770'
    if ($dialog -eq [IntPtr]::Zero -or [LCDSirPlus.NativeWindow]::Text($dialog) -cne 'LCDSirPlus startup error') { throw 'invalid no-argument launch did not show its native startup error' }
    if ([LCDSirPlus.NativeWindow]::ConsoleProcesses() -contains [uint32]$process.Id) { throw 'invalid no-argument launch attached to the parent console' }
    Stop-TestProcess $process $dialog
    if ($process.ExitCode -ne 2) { throw "invalid GUI startup returned $($process.ExitCode), expected 2" }
}
finally {
    Stop-TestProcess $process $dialog
    [IO.Directory]::Delete($errorRoot, $true)
}

Write-Host ''
Write-Host 'ALL GATES PASSED' -ForegroundColor Green
Write-Host 'Physical acceptance (human) is intentionally NOT part of this script:'
Write-Host '  .\target\release\LCDSirPlus.exe --hardware-test --backend hid --duration-secs 60'
Write-Host '  .\target\release\LCDSirPlus.exe --hardware-test --backend sdk --duration-secs 60  # with LGS running'
