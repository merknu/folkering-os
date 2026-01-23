# Test boot script for Folkering OS kernel
# Requires QEMU to be installed

$ErrorActionPreference = "Stop"

Write-Host "Building kernel..." -ForegroundColor Cyan
cargo build --target x86_64-unknown-none
if ($LASTEXITCODE -ne 0) {
    Write-Host "Build failed!" -ForegroundColor Red
    exit 1
}

Write-Host "Copying kernel to ISO root..." -ForegroundColor Cyan
Copy-Item -Force "target\x86_64-unknown-none\debug\kernel" "iso_root\boot\kernel.elf"

Write-Host "Creating temporary disk image..." -ForegroundColor Cyan
# Create a 100MB disk image
$diskSize = 100MB
$diskPath = "test-disk.img"
$fso = New-Object -ComObject Scripting.FileSystemObject
$file = $fso.CreateTextFile($diskPath, $true)
$file.Close()
[System.IO.File]::WriteAllBytes($diskPath, (New-Object byte[] $diskSize))

Write-Host "Testing boot with QEMU..." -ForegroundColor Cyan
Write-Host "Note: VGA output should show 'HELLO' in white on red background" -ForegroundColor Yellow
Write-Host "Serial output will appear below:" -ForegroundColor Yellow
Write-Host "=" * 60

& qemu-system-x86_64 `
    -drive "file=iso_root,format=raw,if=virtio" `
    -serial "file:serial.log" `
    -display none `
    -no-reboot `
    -no-shutdown `
    -m 512M `
    -cpu qemu64 `
    -smp 1 `
    -d cpu_reset,guest_errors

Write-Host "=" * 60
Write-Host "`nSerial output:" -ForegroundColor Cyan
if (Test-Path "serial.log") {
    Get-Content "serial.log"
} else {
    Write-Host "No serial output captured" -ForegroundColor Yellow
}
