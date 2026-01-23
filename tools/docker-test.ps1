# Docker-based boot test for Folkering OS kernel (Windows)

$ErrorActionPreference = "Stop"

Write-Host "Building kernel..." -ForegroundColor Cyan
cargo build --target x86_64-unknown-none
if ($LASTEXITCODE -ne 0) { exit 1 }

Write-Host "Copying kernel to ISO root..." -ForegroundColor Cyan
Copy-Item -Force "target\x86_64-unknown-none\debug\kernel" "iso_root\boot\kernel.elf"

Write-Host "Building Docker test image..." -ForegroundColor Cyan
docker build -t folkering-test -f Dockerfile.test .
if ($LASTEXITCODE -ne 0) { exit 1 }

Write-Host "Creating bootable disk image..." -ForegroundColor Cyan
docker run --rm -v "${PWD}:/test" -w /test ubuntu:22.04 bash -c @"
    set -e
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq && apt-get install -y -qq xorriso mtools dosfstools > /dev/null 2>&1

    # Create a 100MB disk image
    dd if=/dev/zero of=boot.img bs=1M count=100 2>/dev/null

    # Format as FAT32
    mkfs.fat -F 32 boot.img > /dev/null

    # Create temp mount point
    mkdir -p /mnt/disk
    mount -o loop boot.img /mnt/disk

    # Copy boot files
    cp -r iso_root/* /mnt/disk/

    # Unmount
    umount /mnt/disk
    rmdir /mnt/disk

    echo 'Disk image created successfully'
"@

if ($LASTEXITCODE -ne 0) {
    Write-Host "Failed to create disk image" -ForegroundColor Red
    exit 1
}

Write-Host ""
Write-Host "Running QEMU boot test..." -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Yellow
Write-Host ""

docker run --rm -it `
    -v "${PWD}:/test" `
    -w /test `
    folkering-test `
    -drive file=boot.img,format=raw,if=virtio `
    -serial stdio `
    -no-reboot `
    -no-shutdown `
    -m 512M `
    -cpu qemu64 `
    -smp 1 `
    -display none

Write-Host ""
Write-Host "========================================" -ForegroundColor Yellow
Write-Host "Test complete" -ForegroundColor Cyan
