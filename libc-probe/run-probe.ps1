$ErrorActionPreference = "Continue"

$cells = @(
    @{ crate = "p186"; bin = "localtime" },
    @{ crate = "p186"; bin = "time" },
    @{ crate = "p186"; bin = "ctime" },
    @{ crate = "p187"; bin = "localtime" },
    @{ crate = "p187"; bin = "time" },
    @{ crate = "p187"; bin = "ctime" },
    @{ crate = "p188"; bin = "localtime" },
    @{ crate = "p188"; bin = "time" },
    @{ crate = "p188"; bin = "ctime" },
    @{ crate = "plain"; bin = "plain" }
)

$results = @()
foreach ($c in $cells) {
    $crate = $c.crate
    $bin = $c.bin
    Write-Host ""
    Write-Host "=== building $crate/$bin"
    $out = & cargo build --manifest-path "libc-probe/$crate/Cargo.toml" --bin $bin 2>&1 | Out-String
    if ($LASTEXITCODE -eq 0) {
        $status = "LINK-OK"
        $exe = "libc-probe/$crate/target/debug/$bin.exe"
        $run = & $exe 2>&1 | Out-String
        $detail = "runs: " + $run.Trim()
    }
    else {
        $status = "LINK-FAIL"
        $lnk = ($out -split "`n" | Select-String "LNK2019|LNK1120" | ForEach-Object { $_.Line.Trim() } | Sort-Object -Unique) -join " ; "
        $detail = $lnk
    }
    Write-Host "--- $crate/$bin -> $status"
    Write-Host "    $detail"
    $results += [pscustomobject]@{ cell = "$crate/$bin"; status = $status; detail = $detail }
}

Write-Host ""
Write-Host "================ RESULT MATRIX ($env:PROBE_LABEL) ================"
$results | Format-Table -AutoSize -Wrap | Out-String | Write-Host

$md = "### probe results: $env:PROBE_LABEL`n`n| cell | status | detail |`n|---|---|---|`n"
foreach ($r in $results) {
    $clean = ($r.detail -replace "\|", "\|") -replace "`r?`n", " "
    $md += "| $($r.cell) | $($r.status) | $clean |`n"
}
Add-Content -Path $env:GITHUB_STEP_SUMMARY -Value $md
exit 0
