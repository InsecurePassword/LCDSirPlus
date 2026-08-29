# LCDSirPlus 0.3.0 build and test script (PowerShell 7+)
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

Write-Host '== PowerShell syntax ==' -ForegroundColor Cyan
$parse = '$tokens=$null; $errors=$null; [Management.Automation.Language.Parser]::ParseFile($env:LCDSIRPLUS_PARSE_FILE,[ref]$tokens,[ref]$errors) | Out-Null; if ($errors.Count -ne 0) { $errors | ForEach-Object { Write-Error $_ }; exit 1 }'
$allScripts = @(Get-ChildItem -LiteralPath $PSScriptRoot -Filter '*.ps1' -File | ForEach-Object { $_.Name } | Sort-Object)
foreach ($script in $allScripts) {
    $env:LCDSIRPLUS_PARSE_FILE = Join-Path $PSScriptRoot $script
    & (Get-Command pwsh).Source -NoProfile -Command $parse
    if ($LASTEXITCODE -ne 0) { throw "pwsh syntax failed: $script" }
}
foreach ($script in @('Install.ps1', 'Package.Common.ps1', 'Uninstall.ps1')) {
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
$trackedNames = @(git ls-files --cached --others --exclude-standard | Where-Object { Test-Path -LiteralPath $_ })
if ($LASTEXITCODE -ne 0 -or @($trackedNames | Where-Object { $_ -match $retiredPattern }).Count -ne 0) { throw 'tracked filename contains the retired product identity' }
foreach ($name in $trackedNames) {
    if ([IO.File]::ReadAllText((Join-Path $repo $name)).ToLowerInvariant() -match $retiredPattern) { throw "tracked content contains the retired product identity: $name" }
}

Write-Host '== documentation ==' -ForegroundColor Cyan
$configSource = [IO.File]::ReadAllText((Join-Path $repo 'src\config.rs'))
$moduleBlock = [regex]::Match($configSource, 'pub const MODULES:\s*&\[&str\]\s*=\s*&\[(.*?)\];', [Text.RegularExpressions.RegexOptions]::Singleline)
if (-not $moduleBlock.Success) { throw 'canonical module registry not found' }
$canonicalModules = @([regex]::Matches($moduleBlock.Groups[1].Value, '"([A-Z][A-Z0-9_]*)"') | ForEach-Object { $_.Groups[1].Value })
$moduleLines = @([IO.File]::ReadAllLines((Join-Path $repo 'modules.md')) | Where-Object { $_ -match '^([A-Z][A-Z0-9_]*) - .+$' })
$documentedModules = @($moduleLines | ForEach-Object { [regex]::Match($_, '^([A-Z][A-Z0-9_]*) - ').Groups[1].Value })
if ($canonicalModules.Count -ne 53 -or $documentedModules.Count -ne 53) { throw 'module reference must contain exactly 53 canonical entries' }
for ($index = 0; $index -lt $canonicalModules.Count; $index++) {
    if ($canonicalModules[$index] -cne $documentedModules[$index]) { throw "module reference order mismatch at index $index" }
}
$readme = [IO.File]::ReadAllText((Join-Path $repo 'README.md'))
$manual = [IO.File]::ReadAllText((Join-Path $repo 'docs\INSTRUCTION-MANUAL.md'))
if ($readme -notmatch '\]\(modules\.md\)' -or $manual -notmatch '\]\(\.\./modules\.md\)') { throw 'quick-reference links are missing' }
foreach ($module in $canonicalModules) {
    if (@([regex]::Matches($manual, "(?m)^\| ``$([regex]::Escape($module))`` \|")).Count -ne 1) { throw "instruction manual entry mismatch: $module" }
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
cargo test --release
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
