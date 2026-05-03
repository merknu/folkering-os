# test_mouse_freeze.ps1 — Automated repro for Issue #15 (GUI freeze on mouse motion).
#
# Boots Folkering OS in a fresh QEMU, then injects mouse motion via QMP
# `input-send-event` (no manual VNC viewer needed) and measures the latency
# between each event and its `[M]` marker arriving on the serial log.
#
# Usage:
#   .\test_mouse_freeze.ps1                 # default: WHPX, VNC :1, 30 sweep events
#   .\test_mouse_freeze.ps1 -Accel tcg      # rule out WHPX
#   .\test_mouse_freeze.ps1 -BurstCount 200 # stress the input ring
#   .\test_mouse_freeze.ps1 -KeepRunning    # leave QEMU running for follow-up

[CmdletBinding()]
param(
    [ValidateSet('whpx', 'tcg')]
    [string]$Accel = 'whpx',
    [int]$VncDisplay = 1,                # 1 → TCP 5901
    [int]$QmpPort = 4445,
    [int]$SweepCount = 30,
    [int]$SweepIntervalMs = 100,         # ~10 events/s, like real mouse motion
    [int]$BurstCount = 100,
    [int]$BootTimeoutSec = 180,          # TCG needs ~3 min to boot
    [ValidateSet('qmp', 'vnc')]
    [string]$InjectVia = 'vnc',          # vnc = real RFB PointerEvents (Issue #15 path)
    [switch]$KeepRunning
)

$ErrorActionPreference = 'Stop'
$FolkeringDir = 'C:\Users\merkn\folkering\folkering-os'
$BootImg      = Join-Path $FolkeringDir 'boot\current.img'
$DataImg      = Join-Path $FolkeringDir 'boot\virtio-data.img'
$SerialLog    = Join-Path $FolkeringDir 'tools\mouse_freeze_serial.log'
$StderrLog    = Join-Path $FolkeringDir 'tools\mouse_freeze_stderr.log'
$QemuExe      = 'C:\Program Files\qemu\qemu-system-x86_64.exe'

function Write-Phase($msg) { Write-Host "`n=== $msg ===" -ForegroundColor Cyan }
function Write-Info($msg)  { Write-Host "    $msg" -ForegroundColor DarkGray }
function Write-Ok($msg)    { Write-Host "    $msg" -ForegroundColor Green }
function Write-Warn($msg)  { Write-Host "    $msg" -ForegroundColor Yellow }
function Write-Bad($msg)   { Write-Host "    $msg" -ForegroundColor Red }

# ---- Phase 1: cleanup ------------------------------------------------------
Write-Phase 'Cleanup'
Get-Process -Name 'qemu-system-x86_64' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
'' | Set-Content $SerialLog -Force
'' | Set-Content $StderrLog -Force

# Verify our chosen ports are bindable.
foreach ($port in @($QmpPort, (5900 + $VncDisplay))) {
    try {
        $l = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Any, $port)
        $l.Start(); $l.Stop()
    } catch {
        Write-Bad "Port $port is wedged. Pick another (-VncDisplay or -QmpPort)."
        throw
    }
}
Write-Ok "Ports clear: QMP=$QmpPort  VNC=$(5900 + $VncDisplay)"

# ---- Phase 2: launch QEMU --------------------------------------------------
Write-Phase 'Launch QEMU'
$qemuArgs = @(
    '-drive', "file=$BootImg,format=raw,if=ide",
    '-drive', "file=$DataImg,format=raw,if=none,id=vdisk0",
    '-device', 'virtio-blk-pci,drive=vdisk0',
    '-vga', 'virtio',
    '-usb', '-device', 'usb-tablet',
    '-accel', $Accel,
    '-cpu', 'qemu64,rdrand=on,+avx2,+fma,+avx,+sse4.1,+sse4.2',
    '-smp', '4',
    '-m', '2048M',
    '-qmp', "tcp:127.0.0.1:${QmpPort},server,nowait",
    '-serial', "file:$SerialLog",
    '-display', "vnc=0.0.0.0:$VncDisplay",
    '-no-reboot'
)
# WHPX prefers the kernel-irqchip=off variant on i440fx. Mirror what MCP does:
if ($Accel -eq 'whpx') {
    $idx = $qemuArgs.IndexOf('-accel') + 1
    $qemuArgs[$idx] = 'whpx,kernel-irqchip=off'
}
$proc = Start-Process -FilePath $QemuExe -ArgumentList $qemuArgs -PassThru -RedirectStandardError $StderrLog -NoNewWindow
Write-Ok "QEMU PID=$($proc.Id)  accel=$Accel"

# ---- Phase 3: wait for compositor-ready ------------------------------------
Write-Phase 'Wait for compositor-ready marker'
$ready = $false
$deadline = (Get-Date).AddSeconds($BootTimeoutSec)
while ((Get-Date) -lt $deadline) {
    if ($proc.HasExited) {
        Write-Bad "QEMU died (exit=$($proc.ExitCode))"
        Write-Bad "stderr: $(Get-Content $StderrLog -Raw)"
        throw 'QEMU exited during boot'
    }
    $log = Get-Content $SerialLog -Raw -ErrorAction SilentlyContinue
    if ($log -match '\[COMPOSITOR\] Mouse\+IPC ready') {
        $ready = $true; break
    }
    Start-Sleep -Milliseconds 500
}
if (-not $ready) {
    Write-Bad "Boot timed out after ${BootTimeoutSec}s — last serial:"
    Get-Content $SerialLog -Tail 20 | ForEach-Object { Write-Info $_ }
    if (-not $KeepRunning) { Stop-Process -Id $proc.Id -Force }
    throw 'Boot timeout'
}
Write-Ok 'Compositor ready.'

# ---- Phase 4: QMP handshake ------------------------------------------------
function Open-Qmp {
    param([int]$Port)
    $client = [System.Net.Sockets.TcpClient]::new()
    $client.Connect('127.0.0.1', $Port)
    $stream = $client.GetStream()
    $reader = [System.IO.StreamReader]::new($stream, [System.Text.Encoding]::UTF8)
    $writer = [System.IO.StreamWriter]::new($stream, [System.Text.Encoding]::UTF8)
    $writer.NewLine = "`n"
    $writer.AutoFlush = $true
    # Greeting line.
    $reader.ReadLine() | Out-Null
    # Negotiate.
    $writer.WriteLine('{"execute":"qmp_capabilities"}')
    $reader.ReadLine() | Out-Null
    return [pscustomobject]@{ Client = $client; Reader = $reader; Writer = $writer }
}

function Send-Qmp {
    param($Conn, [string]$Json)
    $Conn.Writer.WriteLine($Json)
    return $Conn.Reader.ReadLine()
}

if ($InjectVia -eq 'vnc') {
    Write-Phase "Injecting via VNC PointerEvents (RFB; mirrors a real viewer)"
    $vncPort = 5900 + $VncDisplay
    $py = Get-Command py -ErrorAction SilentlyContinue
    $pyArgs = @('-3.12', "$FolkeringDir\tools\vnc_mouse_probe.py",
                '--host', '127.0.0.1',
                '--port', $vncPort.ToString(),
                '--serial', $SerialLog,
                '--count', $SweepCount.ToString(),
                '--interval-ms', $SweepIntervalMs.ToString())
    if (-not $py) { throw 'py launcher not found — install Python 3.12' }
    & $py.Path $pyArgs
    if (-not $KeepRunning) {
        Write-Phase 'Stopping QEMU'
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    } else {
        Write-Phase "QEMU left running (PID $($proc.Id), VNC :$VncDisplay)"
    }
    Write-Host "`nFull serial log: $SerialLog" -ForegroundColor DarkCyan
    return
}

Write-Phase 'QMP handshake'
$qmp = Open-Qmp -Port $QmpPort
Write-Ok 'QMP capabilities negotiated.'

# Helper: relative mouse delta — routes to PS/2 mouse (which the OS reads).
# usb-tablet only handles `abs`, so this targets the right device automatically.
function New-RelMouseEvent {
    param([int]$Dx, [int]$Dy, [bool]$LeftButton = $false)
    $events = @(
        '{"type":"rel","data":{"axis":"x","value":' + $Dx + '}}',
        '{"type":"rel","data":{"axis":"y","value":' + $Dy + '}}'
    )
    if ($LeftButton) { $events += '{"type":"btn","data":{"button":"left","down":true}}' }
    return '{"execute":"input-send-event","arguments":{"events":[' + ($events -join ',') + ']}}'
}

# Optional: HMP mouse_move via human-monitor-command (the classic path Issue #15
# reported as broken under WHPX). Useful for parity-checking against `rel`.
function New-HmpMouseMove {
    param([int]$Dx, [int]$Dy)
    $cmd = "mouse_move $Dx $Dy"
    return '{"execute":"human-monitor-command","arguments":{"command-line":"' + $cmd + '"}}'
}

# Track [M] markers by counting occurrences in serial log over time.
function Get-MouseMarkerCount {
    $content = Get-Content $SerialLog -Raw -ErrorAction SilentlyContinue
    if (-not $content) { return 0 }
    return ([regex]::Matches($content, '\[M\]')).Count
}

# Snapshot baseline before the experiment.
$baselineMarkers = Get-MouseMarkerCount
Write-Info "Baseline [M] markers: $baselineMarkers"

# ---- Phase 5: sweep test (slow, deliberate motion) -------------------------
Write-Phase "Sweep test ($SweepCount events @ ${SweepIntervalMs}ms intervals)"
$sweepResults = New-Object System.Collections.Generic.List[double]
for ($i = 0; $i -lt $SweepCount; $i++) {
    # Alternate signs so we don't drift off-screen.
    $dx = if ($i % 2 -eq 0) { 5 } else { -5 }
    $dy = if ($i % 2 -eq 0) { 5 } else { -5 }
    $beforeMarkers = Get-MouseMarkerCount
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    Send-Qmp -Conn $qmp -Json (New-RelMouseEvent -Dx $dx -Dy $dy) | Out-Null
    # Wait for [M] count to increment, capped at 5s per event (so a 3-4s
    # WHPX freeze is captured rather than truncated at 2s).
    $eventDeadline = (Get-Date).AddMilliseconds(5000)
    while ((Get-Date) -lt $eventDeadline -and (Get-MouseMarkerCount) -le $beforeMarkers) {
        Start-Sleep -Milliseconds 5
    }
    $sw.Stop()
    $sweepResults.Add($sw.Elapsed.TotalMilliseconds) | Out-Null
    Start-Sleep -Milliseconds $SweepIntervalMs
}

$sweepStats = $sweepResults | Measure-Object -Average -Maximum -Minimum
Write-Info ("Sweep latency (ms):  min={0:F1}  avg={1:F1}  max={2:F1}" -f `
    $sweepStats.Minimum, $sweepStats.Average, $sweepStats.Maximum)

# Highlight events that took >1s (the "freeze" symptom in Issue #15).
$slowEvents = $sweepResults | Where-Object { $_ -gt 1000 }
if ($slowEvents.Count -gt 0) {
    Write-Bad "$($slowEvents.Count)/$SweepCount events took >1000ms — FREEZE REPRODUCES"
} else {
    Write-Ok "All sweep events processed in <1s — no freeze"
}

# ---- Phase 6: burst test (input ring stress) -------------------------------
Write-Phase "Burst test ($BurstCount events back-to-back)"
$beforeBurstMarkers = Get-MouseMarkerCount
$burstSw = [System.Diagnostics.Stopwatch]::StartNew()
for ($i = 0; $i -lt $BurstCount; $i++) {
    $dx = (Get-Random -Minimum -10 -Maximum 11)
    $dy = (Get-Random -Minimum -10 -Maximum 11)
    Send-Qmp -Conn $qmp -Json (New-RelMouseEvent -Dx $dx -Dy $dy) | Out-Null
}
$burstSw.Stop()
$burstSendMs = $burstSw.Elapsed.TotalMilliseconds
Write-Info ("All $BurstCount events sent to QMP in {0:F0}ms" -f $burstSendMs)

# Drain phase: how long until [M] markers stabilize?
$drainSw = [System.Diagnostics.Stopwatch]::StartNew()
$lastMarkers = Get-MouseMarkerCount
$stableTicks = 0
while ($drainSw.Elapsed.TotalSeconds -lt 15) {
    Start-Sleep -Milliseconds 500
    $now = Get-MouseMarkerCount
    if ($now -eq $lastMarkers) { $stableTicks++ } else { $stableTicks = 0 }
    if ($stableTicks -ge 6) { break }   # 3s of no growth = drained
    $lastMarkers = $now
}
$drainSw.Stop()
$burstMarkers = (Get-MouseMarkerCount) - $beforeBurstMarkers
Write-Info ("Burst processed: {0} [M] markers in {1:F1}s of drain" -f $burstMarkers, $drainSw.Elapsed.TotalSeconds)

# ---- Phase 7: TIMING summary from serial -----------------------------------
Write-Phase 'Compositor TIMING samples (last 30)'
$timingLines = (Get-Content $SerialLog) -match '^TIMING,' | Select-Object -Last 30
if ($timingLines.Count -gt 0) {
    $totals = $timingLines | ForEach-Object { ($_ -split ',')[1] -as [int] }
    $totalStats = $totals | Measure-Object -Average -Maximum
    Write-Info ("compositor frame total_us:  avg={0:F0}  max={1}" -f $totalStats.Average, $totalStats.Maximum)
    if ($totalStats.Maximum -gt 100000) {
        Write-Warn "Worst frame >100ms — investigate compositor render path"
    } else {
        Write-Ok 'All frames <100ms — render not the bottleneck'
    }
} else {
    Write-Warn 'No TIMING samples in serial — compositor instrumentation may be off'
}

# ---- Phase 8: cleanup ------------------------------------------------------
$qmp.Client.Close()
if (-not $KeepRunning) {
    Write-Phase 'Stopping QEMU'
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
} else {
    Write-Phase "QEMU left running (PID $($proc.Id), VNC :$VncDisplay)"
}

Write-Host ''
Write-Host "Full serial log: $SerialLog" -ForegroundColor DarkCyan
