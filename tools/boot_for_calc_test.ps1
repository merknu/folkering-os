# boot_for_calc_test.ps1 — Boot Folkering OS in QEMU/WHPX, wait for
# compositor-ready, then leave the VM running for the calc demo client.
#
# Usage: .\tools\boot_for_calc_test.ps1
[CmdletBinding()]
param(
    [int]$VncDisplay = 1,
    [int]$BootTimeoutSec = 180,
    [ValidateSet('whpx', 'tcg')]
    [string]$Accel = 'whpx'
)
$ErrorActionPreference = 'Stop'
$FolkeringDir = 'C:\Users\merkn\folkering\folkering-os'
$BootImg   = Join-Path $FolkeringDir 'boot\current.img'
$DataImg   = Join-Path $FolkeringDir 'boot\virtio-data.img'
$SerialLog = Join-Path $FolkeringDir 'tools\calc_serial.log'
$StderrLog = Join-Path $FolkeringDir 'tools\calc_stderr.log'
$QemuExe   = 'C:\Program Files\qemu\qemu-system-x86_64.exe'

Get-Process -Name 'qemu-system-x86_64' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
'' | Set-Content $SerialLog -Force
'' | Set-Content $StderrLog -Force

$qemuArgs = @(
    '-drive', "file=$BootImg,format=raw,if=ide",
    '-drive', "file=$DataImg,format=raw,if=none,id=vdisk0",
    '-device', 'virtio-blk-pci,drive=vdisk0',
    '-vga', 'virtio',
    '-usb', '-device', 'usb-tablet',
    '-accel', $(if ($Accel -eq 'whpx') { 'whpx,kernel-irqchip=off' } else { 'tcg' }),
    '-cpu', 'qemu64,rdrand=on,+avx2,+fma,+avx,+sse4.1,+sse4.2',
    '-smp', '4',
    '-m', '2048M',
    '-serial', "file:$SerialLog",
    '-display', "vnc=0.0.0.0:$VncDisplay",
    '-no-reboot'
)
$proc = Start-Process -FilePath $QemuExe -ArgumentList $qemuArgs -PassThru -RedirectStandardError $StderrLog -NoNewWindow
Write-Host "QEMU PID=$($proc.Id) VNC=:$VncDisplay" -ForegroundColor Cyan

$deadline = (Get-Date).AddSeconds($BootTimeoutSec)
$ready = $false
while ((Get-Date) -lt $deadline) {
    if ($proc.HasExited) {
        Write-Host "QEMU died (exit=$($proc.ExitCode))" -ForegroundColor Red
        Get-Content $StderrLog -Raw | Write-Host
        exit 1
    }
    $log = Get-Content $SerialLog -Raw -ErrorAction SilentlyContinue
    if ($log -match '\[FOLKUI-DEMO\] input ring shmem=') {
        $ready = $true; break
    }
    Start-Sleep -Milliseconds 500
}
if (-not $ready) {
    Write-Host "Timed out waiting for [FOLKUI-DEMO] input ring marker" -ForegroundColor Yellow
    Get-Content $SerialLog -Tail 30 | Write-Host
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    exit 1
}
Write-Host "folkui-demo ready. Run: py -3.12 tools\vnc_calc_demo.py --serial tools\calc_serial.log" -ForegroundColor Green
Write-Host "PID $($proc.Id) — kill with: Stop-Process -Id $($proc.Id) -Force" -ForegroundColor DarkGray
