# Packs the mic chain's CPU report (`cpu_breakdown_report`) for another PC: the test binary, the
# fixtures it reads and a run.cmd that writes report.txt there. Nothing is installed on that PC;
# with Python present, run.ps1 also times ONNX Runtime on the same models (a venv in %TEMP%).
# Output: target/package/cpu-bench-<yyyyMMdd-HHmmss>-<commit>.zip
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root

$exe = cargo test --no-run -p gamersequalizer-mic-chain --release --lib --message-format=json |
  ForEach-Object { $_ | ConvertFrom-Json } |
  Where-Object { $_.reason -eq 'compiler-artifact' -and $_.executable } |
  Select-Object -Last 1 -ExpandProperty executable
if ($LASTEXITCODE -ne 0 -or -not $exe) { throw 'cargo test --no-run failed' }

$stage = Join-Path $root 'target/package/cpu-bench-staging'
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force "$stage/denoise", "$stage/models" | Out-Null
Copy-Item $exe "$stage/mic_chain_tests.exe"
Copy-Item test_data/denoise/*.wav "$stage/denoise/"
Copy-Item crates/gamersequalizer-mic-chain/models/dpdfnet2_48khz_hr.onnx "$stage/models/"
tar -xzf crates/vendor/deep_filter/models/DeepFilterNet3_onnx.tar.gz -C "$stage/models"
if ($LASTEXITCODE -ne 0) { throw 'tar failed' }
Move-Item "$stage/models/tmp/export/*.onnx" "$stage/models/"
Remove-Item "$stage/models/tmp" -Recurse -Force

Set-Content "$stage/ort_bench.py" -Encoding ascii -Value @'
import sys, time
import numpy as np
import onnxruntime as ort

o = ort.SessionOptions()
o.intra_op_num_threads = o.inter_op_num_threads = 1
rng = np.random.default_rng(1)
for path in sys.argv[1:]:
    s = ort.InferenceSession(path, o, providers=["CPUExecutionProvider"])
    feed = {i.name: (rng.standard_normal([d if isinstance(d, int) else 1 for d in i.shape]) * 0.1).astype(np.float32)
            for i in s.get_inputs()}
    for _ in range(200):
        s.run(None, feed)
    best = []
    for _ in range(3):
        t = time.perf_counter()
        for _ in range(2000):
            s.run(None, feed)
        best.append((time.perf_counter() - t) / 2000 * 1e6)
    print(f"ORT {path.replace(chr(92), '/').split('/')[-1]}: {min(best):.1f} us/frame (runs {', '.join(f'{b:.1f}' for b in best)})")
'@

# Windows PowerShell 5.1 on the target PC: no ?? / ternary, stderr through cmd so it is not an error record.
Set-Content "$stage/run.ps1" -Encoding utf8 -Value @'
$ErrorActionPreference = 'Continue'
$here = $PSScriptRoot
$out = Join-Path $here 'report.txt'
# Tee-Object in 5.1 writes UTF-16; Add-Content with an encoding keeps report.txt readable anywhere.
function Log($text) { Write-Output $text; Add-Content -Path $out -Value $text -Encoding utf8 }
# Runs append to one report.txt; each starts with its own "== System <time>" header.
if (Test-Path $out) { Log ''; Log ('=' * 60) }

Log "== System $(Get-Date -Format s)"
Get-CimInstance Win32_Processor | ForEach-Object { Log "CPU: $($_.Name), $($_.NumberOfCores) cores / $($_.NumberOfLogicalProcessors) threads, max $($_.MaxClockSpeed) MHz" }
Get-CimInstance Win32_OperatingSystem | ForEach-Object { Log "OS: $($_.Caption) build $($_.BuildNumber), RAM $([math]::Round($_.TotalVisibleMemorySize / 1MB, 1)) GB" }
$bat = Get-CimInstance Win32_Battery
if ($bat) { Log "Battery: status $($bat.BatteryStatus) (2 = on AC), charge $($bat.EstimatedChargeRemaining) %" } else { Log 'Battery: none' }
Log "Power plan: $(powercfg /getactivescheme)"
$ov = Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Power\User\PowerSchemes' -ErrorAction SilentlyContinue
if ($ov.ActiveOverlayAcPowerScheme) { Log "Power mode overlay (AC): $($ov.ActiveOverlayAcPowerScheme)" }

$env:DENOISE_FIXTURES = Join-Path $here 'denoise'
$test = 'ai_denoiser::tests::cpu_breakdown_report'
# COLD_CACHE+PACED: live audio while a game churns the cache.
# Model weights as f32 (GE_F16_WEIGHTS=0), then f16 as shipped (docs/mic-cpu-analysis.md chapter 8).
foreach ($weights in @('f32', 'f16')) {
  if ($weights -eq 'f32') { $env:GE_F16_WEIGHTS = '0' } else { Remove-Item Env:GE_F16_WEIGHTS -ErrorAction SilentlyContinue }
  foreach ($mode in @('warm', 'COLD_CACHE', 'PACED', 'COLD_CACHE+PACED')) {
    Remove-Item Env:COLD_CACHE, Env:PACED -ErrorAction SilentlyContinue
    if ($mode -ne 'warm') { $mode -split '\+' | ForEach-Object { Set-Item "Env:$_" '1' } }
    Log ''
    Log "== tract, weights $weights, $mode"
    cmd /c "`"$here\mic_chain_tests.exe`" $test --exact --ignored --nocapture --test-threads=1 2>&1" |
      Where-Object { $_ -match 'ISA tract|^CPU |panicked|error' } | ForEach-Object { Log $_ }
  }
}
Remove-Item Env:COLD_CACHE, Env:PACED, Env:GE_F16_WEIGHTS -ErrorAction SilentlyContinue

Log ''
Log '== ONNX Runtime 1.32.0 (optional, needs Python 3 and internet for pip)'
$py = Get-Command py, python -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $py) {
  Log 'skipped: no Python'
} else {
  $venv = Join-Path $env:TEMP 'gamersequalizer-ort-bench'
  if (-not (Test-Path "$venv\Scripts\python.exe")) { & $py.Source -m venv $venv }
  & "$venv\Scripts\python.exe" -m pip install --quiet onnxruntime==1.32.0 numpy 2>&1 | Out-Null
  $m = Join-Path $here 'models'
  & "$venv\Scripts\python.exe" -I (Join-Path $here 'ort_bench.py') "$m\enc.onnx" "$m\erb_dec.onnx" "$m\df_dec.onnx" "$m\dpdfnet2_48khz_hr.onnx" 2>&1 |
    ForEach-Object { Log "$_" }
  Log "(venv left in $venv, delete it when done)"
}
Log ''
Log "Done. Send $out"
'@

Set-Content "$stage/run.cmd" -Encoding ascii -Value @'
@echo off
echo Plug the notebook into power, close other apps, then press a key. Takes a few minutes.
pause >nul
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0run.ps1"
pause
'@

$commit = git rev-parse --short HEAD
$zip = Join-Path $root "target/package/cpu-bench-$(Get-Date -Format yyyyMMdd-HHmmss)-$commit.zip"
Compress-Archive -Path "$stage/*" -DestinationPath $zip
Write-Output $zip
