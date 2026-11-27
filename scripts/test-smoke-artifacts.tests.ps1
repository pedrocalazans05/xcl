# Run with powershell.exe or pwsh.exe. Tests scripts/smoke.ps1 -OutDir artifact saving.
$ErrorActionPreference = 'Stop'
$runner = Join-Path $PSScriptRoot 'smoke.ps1'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('xcl-smoke-test-' + [guid]::NewGuid())
$null = New-Item -ItemType Directory -Path $fixture
$mockXcl = Join-Path $fixture 'mock_xcl.cmd'
$gameDir = Join-Path $fixture 'MockGame'
$null = New-Item -ItemType Directory -Path $gameDir
$runDir = Join-Path $fixture 'run'
$null = New-Item -ItemType Directory -Path $runDir
$mockLog = Join-Path $runDir 'mock.log'
$mockReport = Join-Path $runDir 'report.json'
$outDir1 = Join-Path $fixture 'artifacts1'
$outDir2 = Join-Path $fixture 'artifacts2'

try {
    # Generate mock log and report
    @"
[00:00:01] activity (2 s): 30 presents
[00:00:03] activity (2 s): 30 presents
[00:00:05] activity (2 s): 30 presents
[00:00:05] memory: 128 MB committed
(90 frames shown, 90 PresentX calls)
"@ | Set-Content -LiteralPath $mockLog -Encoding utf8

    '{"d3d12": {"adapter": "Mock Synthetic Adapter (16384 MB VRAM)"}}' |
        Set-Content -LiteralPath $mockReport -Encoding utf8

    # Mock XCL script outputs Log: path
    @"
@echo off
echo Running mock game...
echo Log: $mockLog
exit /b 0
"@ | Set-Content -LiteralPath $mockXcl -Encoding Ascii

    # Run 1: with report.json present
    $output1 = & $runner -Game $gameDir -Xcl $mockXcl -Seconds 5 -OutDir $outDir1 -Title 'MockRun1'
    if ($LASTEXITCODE -ne 0) { throw "smoke.ps1 exited with code $LASTEXITCODE on run 1" }

    if (-not (Test-Path (Join-Path $outDir1 'xcl.log'))) { throw 'run 1 did not preserve xcl.log' }
    if (-not (Test-Path (Join-Path $outDir1 'report.json'))) { throw 'run 1 did not preserve report.json' }
    if (-not (Test-Path (Join-Path $outDir1 'manifest.json'))) { throw 'run 1 did not write manifest.json' }

    $manifest1 = Get-Content (Join-Path $outDir1 'manifest.json') -Raw -Encoding utf8 | ConvertFrom-Json
    if ($manifest1.gpu -ne 'Mock Synthetic Adapter') { throw "unexpected gpu: $($manifest1.gpu)" }
    if ($manifest1.seconds -ne 5) { throw "unexpected seconds: $($manifest1.seconds)" }
    if ($manifest1.frames -ne '90 (PresentX 90)') { throw "unexpected frames: $($manifest1.frames)" }
    if ($manifest1.title -ne 'MockRun1') { throw "unexpected title: $($manifest1.title)" }

    $smokeBlock = $output1 -join "`n"
    if ($smokeBlock -notmatch 'xcl-smoke v1') { throw "smoke block missing header: $smokeBlock" }
    if ($smokeBlock -notmatch 'gpu: Mock Synthetic Adapter') { throw "smoke block missing mock gpu: $smokeBlock" }

    # Run 2: remove report.json to test missing report case; reuse $outDir1 to verify stale report.json is purged
    Remove-Item -LiteralPath $mockReport -Force
    $output2 = & $runner -Game $gameDir -Xcl $mockXcl -Seconds 10 -OutDir $outDir1 -Title 'MockRun2'
    if ($LASTEXITCODE -ne 0) { throw "smoke.ps1 exited with code $LASTEXITCODE on run 2" }

    if (-not (Test-Path (Join-Path $outDir1 'xcl.log'))) { throw 'run 2 did not preserve xcl.log' }
    if (Test-Path (Join-Path $outDir1 'report.json')) { throw 'run 2 did not clean up stale report.json' }
    if (-not (Test-Path (Join-Path $outDir1 'manifest.json'))) { throw 'run 2 did not write manifest.json' }

    $manifest2 = Get-Content (Join-Path $outDir1 'manifest.json') -Raw -Encoding utf8 | ConvertFrom-Json
    if ($manifest2.seconds -ne 10) { throw "unexpected seconds: $($manifest2.seconds)" }
    if ($manifest2.title -ne 'MockRun2') { throw "unexpected title: $($manifest2.title)" }
    if ($manifest2.crashes -ne 0) { throw "expected 0 crashes on clean run, got $($manifest2.crashes)" }

    # Run 3: single crash logging both 'unhandled exception' and 'process ends with status' must count as 1
    @"
[00:00:01] unhandled exception 0xc0000005 in game.exe+0x1234
[00:00:01] process ends with status 0xc0000005 (thread 1234); on the stack: game.exe+0x1234
"@ | Set-Content -LiteralPath $mockLog -Encoding utf8
    $output3 = & $runner -Game $gameDir -Xcl $mockXcl -Seconds 5 -OutDir $outDir2 -Title 'CrashBothLines'
    if ($LASTEXITCODE -ne 0) { throw "smoke.ps1 exited with code $LASTEXITCODE on run 3" }
    $manifest3 = Get-Content (Join-Path $outDir2 'manifest.json') -Raw -Encoding utf8 | ConvertFrom-Json
    if ($manifest3.crashes -ne 1) { throw "expected 1 crash when both exception and exit lines appear, got $($manifest3.crashes)" }
    $smokeBlock3 = $output3 -join "`n"
    if ($smokeBlock3 -notmatch 'crashes: 1') { throw "smoke block expected 'crashes: 1', got: $smokeBlock3" }

    # Run 4: abnormal termination without unhandled exception (e.g., abort) still counts as 1
    @"
[00:00:02] process ends with status 0x3 (thread 5678); on the stack: ucrtbase.dll+0x100
"@ | Set-Content -LiteralPath $mockLog -Encoding utf8
    $null = & $runner -Game $gameDir -Xcl $mockXcl -Seconds 5 -OutDir $outDir2 -Title 'CrashExitOnly'
    $manifest4 = Get-Content (Join-Path $outDir2 'manifest.json') -Raw -Encoding utf8 | ConvertFrom-Json
    if ($manifest4.crashes -ne 1) { throw "expected 1 crash for abnormal exit alone, got $($manifest4.crashes)" }

    # Run 3: caller env vars (XCL_NETWORK, XCL_RUN_SECONDS, XCL_ACTIVITY) must be preserved when pre-set
    $env:XCL_NETWORK = '1'
    $env:XCL_RUN_SECONDS = '777'
    $env:XCL_ACTIVITY = '0'
    $null = & $runner -Game $gameDir -Xcl $mockXcl -Seconds 5 -Title 'EnvPresetOk'
    if ($env:XCL_NETWORK -ne '1') { throw "XCL_NETWORK mutated after success: $($env:XCL_NETWORK)" }
    if ($env:XCL_RUN_SECONDS -ne '777') { throw "XCL_RUN_SECONDS mutated after success: $($env:XCL_RUN_SECONDS)" }
    if ($env:XCL_ACTIVITY -ne '0') { throw "XCL_ACTIVITY mutated after success: $($env:XCL_ACTIVITY)" }

    # Run 4: caller env vars must still be preserved when smoke.ps1 exits with an error
    $badXcl = Join-Path $fixture 'bad_xcl.cmd'
    "@echo off`r`necho no log line here`r`nexit /b 1`r`n" | Set-Content -LiteralPath $badXcl -Encoding Ascii
    $threw = $false
    try {
        $null = & $runner -Game $gameDir -Xcl $badXcl -Seconds 5 -Title 'EnvPresetErr'
    } catch {
        $threw = $true
    }
    if (-not $threw) { throw 'expected smoke.ps1 to throw when XCL prints no Log: line' }
    if ($env:XCL_NETWORK -ne '1') { throw "XCL_NETWORK mutated after error: $($env:XCL_NETWORK)" }
    if ($env:XCL_RUN_SECONDS -ne '777') { throw "XCL_RUN_SECONDS mutated after error: $($env:XCL_RUN_SECONDS)" }
    if ($env:XCL_ACTIVITY -ne '0') { throw "XCL_ACTIVITY mutated after error: $($env:XCL_ACTIVITY)" }

    # Run 5: unset env vars must remain unset after both success and error
    Remove-Item Env:XCL_NETWORK, Env:XCL_RUN_SECONDS, Env:XCL_ACTIVITY -ErrorAction SilentlyContinue
    $null = & $runner -Game $gameDir -Xcl $mockXcl -Seconds 5 -Title 'EnvUnsetOk'
    foreach ($name in @('XCL_NETWORK', 'XCL_RUN_SECONDS', 'XCL_ACTIVITY')) {
        if (Test-Path "Env:$name") { throw "$name leaked into caller session after success" }
    }
    $threw = $false
    try {
        $null = & $runner -Game $gameDir -Xcl $badXcl -Seconds 5 -Title 'EnvUnsetErr'
    } catch {
        $threw = $true
    }
    if (-not $threw) { throw 'expected smoke.ps1 to throw on bad_xcl (unset env)' }
    foreach ($name in @('XCL_NETWORK', 'XCL_RUN_SECONDS', 'XCL_ACTIVITY')) {
        if (Test-Path "Env:$name") { throw "$name leaked into caller session after error" }
    }

    Write-Host "test-smoke-artifacts: all assertions passed successfully."
} finally {
    Remove-Item Env:XCL_NETWORK, Env:XCL_RUN_SECONDS, Env:XCL_ACTIVITY -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $fixture) {
        Remove-Item -LiteralPath $fixture -Recurse -Force
    }
}
