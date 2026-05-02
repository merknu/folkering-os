# Probe which QMP/HMP mouse-injection method actually drives the guest's
# PS/2 IRQ12 handler. Boots Folkering OS once, then sends each candidate
# command and counts [M] markers that appear in the serial log.

param(
    [ValidateSet('whpx', 'tcg')]
    [string]$Accel = 'whpx',
    [int]$VncDisplay = 1,
    [int]$QmpPort = 4445
)

$ErrorActionPreference = 'Stop'
$FolkeringDir = 'C:\Users\merkn\folkering\folkering-os'
$BootImg      = Join-Path $FolkeringDir 'boot\current.img'
$DataImg      = Join-Path $FolkeringDir 'boot\virtio-data.img'
$SerialLog    = Join-Path $FolkeringDir 'tools\probe_routing.log'
$QemuExe      = 'C:\Program Files\qemu\qemu-system-x86_64.exe'

Get-Process -Name 'qemu-system-x86_64' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
'' | Set-Content $SerialLog -Force

$accelArg = if ($Accel -eq 'whpx') { 'whpx,kernel-irqchip=off' } else { 'tcg' }
$qemuArgs = @(
    '-drive', "file=$BootImg,format=raw,if=ide",
    '-drive', "file=$DataImg,format=raw,if=none,id=vdisk0",
    '-device', 'virtio-blk-pci,drive=vdisk0',
    '-vga', 'virtio',
    '-usb', '-device', 'usb-tablet,id=mytablet',
    '-accel', $accelArg,
    '-cpu', 'qemu64,rdrand=on,+avx2,+fma,+avx,+sse4.1,+sse4.2',
    '-smp', '4',
    '-m', '2048M',
    '-qmp', "tcp:127.0.0.1:${QmpPort},server,nowait",
    '-serial', "file:$SerialLog",
    '-display', "vnc=0.0.0.0:$VncDisplay",
    '-no-reboot'
)
$proc = Start-Process -FilePath $QemuExe -ArgumentList $qemuArgs -PassThru -NoNewWindow
Write-Host "QEMU PID=$($proc.Id) accel=$Accel" -ForegroundColor Cyan

# Wait up to 90s for compositor ready.
$deadline = (Get-Date).AddSeconds(90)
$ready = $false
while ((Get-Date) -lt $deadline) {
    if ($proc.HasExited) { throw "QEMU died exit=$($proc.ExitCode)" }
    if ((Get-Content $SerialLog -Raw -ErrorAction SilentlyContinue) -match '\[COMPOSITOR\] Mouse\+IPC ready') {
        $ready = $true; break
    }
    Start-Sleep -Milliseconds 500
}
if (-not $ready) { Stop-Process -Id $proc.Id -Force; throw 'Boot timeout' }
Write-Host 'Compositor ready.' -ForegroundColor Green

# QMP setup.
$client = [System.Net.Sockets.TcpClient]::new()
$client.Connect('127.0.0.1', $QmpPort)
$stream = $client.GetStream()
$reader = [System.IO.StreamReader]::new($stream)
$writer = [System.IO.StreamWriter]::new($stream); $writer.NewLine = "`n"; $writer.AutoFlush = $true
$reader.ReadLine() | Out-Null
$writer.WriteLine('{"execute":"qmp_capabilities"}'); $reader.ReadLine() | Out-Null

function Send($json) {
    $writer.WriteLine($json)
    return $reader.ReadLine()
}

function CountM() {
    $c = Get-Content $SerialLog -Raw -ErrorAction SilentlyContinue
    if (-not $c) { return 0 }
    return ([regex]::Matches($c, '\[M\]')).Count
}

function Probe($label, $jsonCmd) {
    Write-Host "`n--- $label ---" -ForegroundColor Yellow
    $before = CountM
    $resp = Send $jsonCmd
    Write-Host "  Sent.    QMP response: $resp"
    Start-Sleep -Seconds 5
    $after = CountM
    $delta = $after - $before
    if ($delta -gt 0) {
        Write-Host "  RESULT: $delta new [M] markers — DELIVERY OK" -ForegroundColor Green
    } else {
        Write-Host "  RESULT: 0 new [M] markers — NOT DELIVERED to guest" -ForegroundColor Red
    }
    return $delta
}

# Test 1: input-send-event with `rel` (should route to PS/2 mouse).
Probe 'input-send-event rel +5,+5' '{"execute":"input-send-event","arguments":{"events":[{"type":"rel","data":{"axis":"x","value":5}},{"type":"rel","data":{"axis":"y","value":5}}]}}' | Out-Null

# Test 2: input-send-event with `abs` (usb-tablet).
Probe 'input-send-event abs 16384,16384' '{"execute":"input-send-event","arguments":{"events":[{"type":"abs","data":{"axis":"x","value":16384}},{"type":"abs","data":{"axis":"y","value":16384}}]}}' | Out-Null

# Test 3: HMP mouse_move via human-monitor-command.
Probe 'HMP mouse_move 5 5' '{"execute":"human-monitor-command","arguments":{"command-line":"mouse_move 5 5"}}' | Out-Null

# Test 4: HMP mouse_move with explicit Mouse #2 (PS/2) selected.
Send '{"execute":"human-monitor-command","arguments":{"command-line":"mouse_set 2"}}' | Out-Null
Probe 'After mouse_set 2 → mouse_move 5 5' '{"execute":"human-monitor-command","arguments":{"command-line":"mouse_move 5 5"}}' | Out-Null

# Test 5: mouse_button click via HMP.
Probe 'HMP mouse_button 1 (left down)' '{"execute":"human-monitor-command","arguments":{"command-line":"mouse_button 1"}}' | Out-Null
Probe 'HMP mouse_button 0 (release)'   '{"execute":"human-monitor-command","arguments":{"command-line":"mouse_button 0"}}' | Out-Null

# Inspect query-mice after our work.
Write-Host "`n--- post-test info mice ---" -ForegroundColor Yellow
Write-Host (Send '{"execute":"human-monitor-command","arguments":{"command-line":"info mice"}}')

$client.Close()
Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
Write-Host "`nFull serial log: $SerialLog" -ForegroundColor DarkCyan
