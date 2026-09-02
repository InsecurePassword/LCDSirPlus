<#
.SYNOPSIS
Builds the PDF instruction manual from its Markdown source.

.DESCRIPTION
Uses PowerShell 7 and an installed Chrome or Edge browser to render docs/INSTRUCTION-MANUAL.md. Creates or atomically replaces docs/LCDSirPlus-Instruction-Manual.pdf and removes temporary files.

.EXAMPLE
PS> .\scripts\Build-Manual.ps1
Renders the repository instruction manual PDF.
#>
#Requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
$source = Join-Path $repo 'docs\INSTRUCTION-MANUAL.md'
$output = Join-Path $repo 'docs\LCDSirPlus-Instruction-Manual.pdf'
$outputDirectory = Split-Path $output -Parent
$id = [Guid]::NewGuid().ToString('N')
$tempHtml = Join-Path ([IO.Path]::GetTempPath()) ('LCDSirPlus-Manual-' + [Guid]::NewGuid().ToString('N') + '.html')
$tempPdf = Join-Path $outputDirectory ('.LCDSirPlus-Manual-' + $id + '.tmp.pdf')
$tempProfile = $null

function Normalize-PdfMetadata([string]$Path) {
    $bytes = [IO.File]::ReadAllBytes($Path)
    $text = [Text.Encoding]::Latin1.GetString($bytes)
    $pattern = '/(CreationDate|ModDate) \(D:\d{14}[+-]\d{2}''\d{2}''\)'
    $matches = [regex]::Matches($text, $pattern)
    $fields = $matches | ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique
    if (($fields -join ',') -ne 'CreationDate,ModDate') {
        throw 'PDF does not contain the expected CreationDate and ModDate metadata'
    }
    $normalized = [regex]::Replace($text, $pattern, {
        param($match)
        '/' + $match.Groups[1].Value + " (D:20000101000000+00'00')"
    })
    if ($normalized.Length -ne $text.Length) {
        throw 'PDF metadata normalization changed the file length'
    }
    [IO.File]::WriteAllBytes($Path, [Text.Encoding]::Latin1.GetBytes($normalized))
}

function Assert-ValidPdf([string]$Path) {
    $bytes = [IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 10240 -or [Text.Encoding]::ASCII.GetString($bytes, 0, 5) -cne '%PDF-') {
        throw 'generated file is not a nontrivial PDF'
    }
    $text = [Text.Encoding]::Latin1.GetString($bytes)
    if (-not $text.Substring([Math]::Max(0, $text.Length - 1024)).Contains('%%EOF')) {
        throw 'generated PDF has no terminal EOF marker'
    }
    if ($text.Contains('file://', [StringComparison]::OrdinalIgnoreCase)) {
        throw 'generated PDF contains a local file URL'
    }
}

try {
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "manual source not found: $source"
    }
    $markdown = ConvertFrom-Markdown -Path $source
    $css = @'
<style>
@page { size: A4; margin: 16mm 14mm 18mm; }
html { font-family: "Segoe UI", Arial, sans-serif; font-size: 10pt; color: #17202a; }
body { max-width: 180mm; margin: 0 auto; line-height: 1.35; }
h1 { font-size: 24pt; border-bottom: 2px solid #283747; padding-bottom: 6px; }
h2 { font-size: 16pt; margin-top: 22px; border-bottom: 1px solid #aab7b8; padding-bottom: 3px; }
h3 { font-size: 12pt; margin-top: 16px; }
h1, h2, h3 { color: #1b4f72; break-after: avoid; }
a { color: #1f618d; text-decoration: none; }
pre, code { font-family: Consolas, "Courier New", monospace; }
code { background: #f2f4f4; padding: 1px 3px; }
pre { background: #f4f6f7; border-left: 3px solid #5d6d7e; padding: 8px; white-space: pre-wrap; break-inside: avoid; }
table { width: 100%; border-collapse: collapse; margin: 8px 0 14px; font-size: 8.2pt; }
th, td { border: 1px solid #aab7b8; padding: 4px 5px; text-align: left; vertical-align: top; }
th { background: #d6eaf8; color: #154360; }
tr { break-inside: avoid; }
blockquote { border-left: 3px solid #85929e; margin-left: 0; padding-left: 10px; }
</style>
'@
    $body = [regex]::Replace(
        $markdown.Html,
        '<a\s+href="(?!(?:#|https?://))[^\"]*"[^>]*>(.*?)</a>',
        '$1',
        [Text.RegularExpressions.RegexOptions]::IgnoreCase -bor [Text.RegularExpressions.RegexOptions]::Singleline
    )
    $html = '<!doctype html><html><head><meta charset="utf-8"><title>LCDSirPlus Instruction Manual</title>' + $css + '</head><body>' + $body + '</body></html>'
    [IO.File]::WriteAllText($tempHtml, $html, [Text.UTF8Encoding]::new($false))

    $browsers = @(
        (Join-Path $env:ProgramFiles 'Google\Chrome\Application\chrome.exe'),
        (Join-Path ${env:ProgramFiles(x86)} 'Google\Chrome\Application\chrome.exe'),
        (Join-Path $env:ProgramFiles 'Microsoft\Edge\Application\msedge.exe'),
        (Join-Path ${env:ProgramFiles(x86)} 'Microsoft\Edge\Application\msedge.exe')
    ) | Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) } | Select-Object -Unique
    if ($browsers.Count -eq 0) {
        throw 'PDF generation requires installed Google Chrome or Microsoft Edge'
    }

    $uri = [Uri]::new($tempHtml).AbsoluteUri
    $errors = @()
    foreach ($browser in $browsers) {
        $process = $null
        $tempProfile = Join-Path $outputDirectory ('.LCDSirPlus-Manual-' + [Guid]::NewGuid().ToString('N') + '.profile')
        try {
            $startInfo = [Diagnostics.ProcessStartInfo]::new($browser)
            $startInfo.UseShellExecute = $false
            foreach ($argument in @(
                '--headless',
                '--disable-gpu',
                '--no-pdf-header-footer',
                "--user-data-dir=$tempProfile",
                "--print-to-pdf=$tempPdf",
                $uri
            )) {
                $startInfo.ArgumentList.Add($argument)
            }
            $process = [Diagnostics.Process]::Start($startInfo)
            if (-not $process.WaitForExit(30000)) {
                $process.Kill($true)
                [void]$process.WaitForExit(5000)
                throw 'timed out after 30 seconds'
            }
            if ($process.ExitCode -ne 0) {
                throw "exited with code $($process.ExitCode)"
            }
            Normalize-PdfMetadata $tempPdf
            Assert-ValidPdf $tempPdf

            if (Test-Path -LiteralPath $output -PathType Leaf) {
                $backup = Join-Path $outputDirectory ('.LCDSirPlus-Manual-' + $id + '.bak.pdf')
                [IO.File]::Replace($tempPdf, $output, $backup, $true)
                Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
            }
            else {
                [IO.File]::Move($tempPdf, $output)
            }
            Write-Host "Manual PDF written to $output" -ForegroundColor Green
            return
        }
        catch {
            $errors += "$browser ($($_.Exception.Message))"
        }
        finally {
            if ($process -and -not $process.HasExited) {
                $process.Kill($true)
                [void]$process.WaitForExit(5000)
            }
            if ($process) {
                $process.Dispose()
            }
            if (Test-Path -LiteralPath $tempPdf) {
                Remove-Item -LiteralPath $tempPdf -Force -ErrorAction SilentlyContinue
            }
            if (Test-Path -LiteralPath $tempProfile) {
                Remove-Item -LiteralPath $tempProfile -Recurse -Force -ErrorAction SilentlyContinue
            }
        }
    }
    throw ('PDF generation failed with: ' + ($errors -join '; '))
}
finally {
    if (Test-Path -LiteralPath $tempHtml) { Remove-Item -LiteralPath $tempHtml -Force -ErrorAction SilentlyContinue }
    if (Test-Path -LiteralPath $tempPdf) { Remove-Item -LiteralPath $tempPdf -Force -ErrorAction SilentlyContinue }
    if (Test-Path -LiteralPath $tempProfile) { Remove-Item -LiteralPath $tempProfile -Recurse -Force -ErrorAction SilentlyContinue }
}
