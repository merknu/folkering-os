# Folkering OS — Phase 5 Hybrid AI Launcher
# Starts Ollama, Serial Proxy, and QEMU in one click

try {

$FolkeringDir = "C:\Users\merkn\folkering\folkering-os"
$SerialLog = "C:\Users\merkn\folkering-mcp\serial.log"

Write-Host "`n=== Folkering OS Launcher ===" -ForegroundColor Cyan
Write-Host ""

# Step 1: Kill ALL old instances (thorough cleanup)
Write-Host "[1/5] Cleaning up old processes..." -ForegroundColor Yellow
Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Get-Process vncviewer64 -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
# Kill old proxy processes
Get-WmiObject Win32_Process -Filter "CommandLine LIKE '%serial-gemini%'" -ErrorAction SilentlyContinue | ForEach-Object { $_.Terminate() } 2>$null
Start-Sleep -Seconds 3
# Verify ports are free
Write-Host "  Waiting for ports to clear..." -ForegroundColor DarkGray
$retries = 0
while ($retries -lt 5) {
    $port4445 = Get-NetTCPConnection -LocalPort 4445 -ErrorAction SilentlyContinue
    $port4567 = Get-NetTCPConnection -LocalPort 4567 -ErrorAction SilentlyContinue
    if (-not $port4445 -and -not $port4567) { break }
    Start-Sleep -Seconds 2
    $retries++
}
Write-Host "  Clean!" -ForegroundColor Green

# Step 2: Check/start Ollama
Write-Host "[2/5] Checking Ollama..." -ForegroundColor Yellow
try {
    $response = Invoke-RestMethod -Uri "http://localhost:11434/api/tags" -TimeoutSec 3 -ErrorAction Stop
    Write-Host "  Ollama is running." -ForegroundColor Green
} catch {
    Write-Host "  Ollama not running. Starting..." -ForegroundColor Yellow
    Start-Process "ollama" -ArgumentList "serve" -WindowStyle Hidden
    Start-Sleep -Seconds 5
    try {
        Invoke-RestMethod -Uri "http://localhost:11434/api/tags" -TimeoutSec 5 -ErrorAction Stop | Out-Null
        Write-Host "  Ollama started!" -ForegroundColor Green
    } catch {
        Write-Host "  WARNING: Could not start Ollama." -ForegroundColor Red
    }
}

# Step 3: Start QEMU with WHPX + VNC (no SDL freeze issue)
Write-Host "[3/5] Starting Folkering OS in QEMU (WHPX)..." -ForegroundColor Yellow

"" | Set-Content $SerialLog -Force -ErrorAction SilentlyContinue

$qemuExe = "C:\Program Files\qemu\qemu-system-x86_64.exe"
$bootImg = "$FolkeringDir\boot\current.img"
$dataImg = "$FolkeringDir\boot\virtio-data.img"

# WHPX + VNC: hypervisor for speed, VNC for input (no SDL event flooding)
$qemuArgs = @(
    "-drive", "file=$bootImg,format=raw,if=ide",
    "-drive", "file=$dataImg,format=raw,if=none,id=vdisk0",
    "-device", "virtio-blk-pci,drive=vdisk0",
    "-netdev", "user,id=net0",
    "-device", "virtio-net-pci,netdev=net0",
    "-vga", "virtio",
    "-usb", "-device", "usb-tablet",
    "-accel", "whpx",
    "-accel", "tcg",
    "-cpu", "qemu64,rdrand=on,+avx2,+fma,+avx,+sse4.1,+sse4.2",
    "-smp", "4",
    "-m", "2048M",
    "-qmp", "tcp:127.0.0.1:4445,server,nowait",
    "-serial", "file:$SerialLog",
    "-serial", "tcp:127.0.0.1:4567,server,nowait",
    "-serial", "tcp:127.0.0.1:4568,server,nowait",
    "-display", "none",
    "-vnc", "0.0.0.0:0",
    "-no-reboot"
)

Start-Process -FilePath $qemuExe -ArgumentList $qemuArgs
Write-Host "  QEMU started (WHPX + VNC)!" -ForegroundColor Green
Start-Sleep -Seconds 3

# Step 4: Start Serial Proxy
Write-Host "[4/5] Starting Serial Proxy (Ollama <-> COM2)..." -ForegroundColor Yellow
$proxyScript = "$FolkeringDir\tools\serial-gemini-proxy.py"
Start-Process -FilePath "py" -ArgumentList "-3.12", "-u", $proxyScript -WorkingDirectory $FolkeringDir -WindowStyle Normal
Write-Host "  Proxy started!" -ForegroundColor Green

# Step 5: Launch TigerVNC viewer
Write-Host "[5/5] Opening TigerVNC..." -ForegroundColor Yellow
$vnc = "C:\Users\merkn\Downloads\vncviewer64.exe"
if (Test-Path $vnc) {
    Start-Sleep -Seconds 3
    Start-Process -FilePath $vnc -ArgumentList "localhost:5900"
    Write-Host "  VNC connected!" -ForegroundColor Green
} else {
    Write-Host "  TigerVNC not found at $vnc" -ForegroundColor Red
    Write-Host "  Connect manually: localhost:5900" -ForegroundColor Yellow
}

Write-Host ""
Write-Host "============================================" -ForegroundColor Green
Write-Host "  Folkering OS is booting!" -ForegroundColor Green
Write-Host "  WHPX accelerated, VNC display." -ForegroundColor Cyan
Write-Host "  Try: gemini explain fibonacci" -ForegroundColor Cyan
Write-Host "============================================" -ForegroundColor Green
Write-Host ""
Write-Host "Press any key to stop everything..." -ForegroundColor DarkGray

$null = $Host.UI.RawUI.ReadKey("NoEcho,IncludeKeyDown")

# Cleanup
Write-Host "`nShutting down..." -ForegroundColor Yellow
Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Get-Process vncviewer64 -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Write-Host "Done!" -ForegroundColor Green
Start-Sleep -Seconds 2

} catch {
    Write-Host "`nERROR: $($_.Exception.Message)" -ForegroundColor Red
    Write-Host "Press any key to close..." -ForegroundColor DarkGray
    $null = $Host.UI.RawUI.ReadKey("NoEcho,IncludeKeyDown")
}
