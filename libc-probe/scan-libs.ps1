$ErrorActionPreference = "Continue"

Write-Host "=== rustc ==="
rustc -Vv

Write-Host "=== Visual Studio ==="
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vsroot = $null
if (Test-Path $vswhere) {
    & $vswhere -latest -property displayName
    & $vswhere -latest -property installationVersion
    $vsroot = & $vswhere -latest -property installationPath
    Write-Host "MSVC toolsets present:"
    Get-ChildItem "$vsroot\VC\Tools\MSVC" -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "  $($_.Name)" }
}

$dumpbin = Get-ChildItem "$vsroot\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe" -ErrorAction SilentlyContinue |
    Sort-Object FullName | Select-Object -Last 1 -ExpandProperty FullName
if (-not $dumpbin) {
    Write-Host "dumpbin.exe not found; skipping import-lib scan"
    exit 0
}
Write-Host "dumpbin: $dumpbin"

$libs = @()
$libs += Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\Lib\*\ucrt\x64\ucrt.lib" -ErrorAction SilentlyContinue
$libs += Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\Lib\*\ucrt\x64\libucrt.lib" -ErrorAction SilentlyContinue
$libs += Get-ChildItem "$vsroot\VC\Tools\MSVC\*\lib\x64\msvcrt.lib" -ErrorAction SilentlyContinue
$libs += Get-ChildItem "$vsroot\VC\Tools\MSVC\*\lib\x64\legacy_stdio_definitions.lib" -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "=== import/static lib symbol scan (localtime|gmtime|ctime|difftime|time) ==="
foreach ($lib in $libs) {
    Write-Host ""
    Write-Host "-- $($lib.FullName)"
    $syms = & $dumpbin /linkermember:1 $lib.FullName 2>$null
    $hits = $syms | Select-String -Pattern "localtime|gmtime|ctime|difftime" |
        ForEach-Object { $_.Line.Trim() } | Sort-Object -Unique
    $timeHits = $syms | Select-String -Pattern "^\s+\S+\s+(__imp_)?_?time(32|64)?$" |
        ForEach-Object { $_.Line.Trim() } | Sort-Object -Unique
    foreach ($h in ($hits + $timeHits)) { Write-Host "   $h" }
    if (-not $hits -and -not $timeHits) { Write-Host "   (no matches)" }
}
exit 0
