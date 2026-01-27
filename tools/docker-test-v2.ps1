# Simplified Docker-based boot test for Folkering OS kernel (Windows)
# Uses mtools instead of mount

$ErrorActionPreference = "Stop"

Write-Host "Building kernel..." -ForegroundColor Cyan
cargo build --target x86_64-unknown-none
if ($LASTEXITCODE -ne 0) { exit 1 }

Write-Host "Copying kernel to ISO root..." -ForegroundColor Cyan
Copy-Item -Force "target\x86_64-unknown-none\debug\kernel" "iso_root\boot\kernel.elf"

# Build folk-pack tool and create initrd
Write-Host "Building folk-pack tool..." -ForegroundColor Cyan
cargo build --manifest-path tools\folk-pack\Cargo.toml
if ($LASTEXITCODE -ne 0) {
    Write-Host "folk-pack build failed, continuing without initrd" -ForegroundColor Yellow
} else {
    $folkPackExe = "tools\folk-pack\target\debug\folk-pack"
    if (Test-Path "$folkPackExe.exe") { $folkPackExe = "$folkPackExe.exe" }

    # Check if userspace shell binary exists
    $shellPath = "userspace\target\x86_64-folkering-userspace\release\shell"
    if (Test-Path $shellPath) {
        Write-Host "Creating initrd.fpk..." -ForegroundColor Cyan
        & $folkPackExe create "iso_root\boot\initrd.fpk" --add "shell:elf:$shellPath"
        if ($LASTEXITCODE -ne 0) {
            Write-Host "folk-pack create failed, continuing without initrd" -ForegroundColor Yellow
        } else {
            Write-Host "initrd.fpk created successfully" -ForegroundColor Green
        }
    } else {
        Write-Host "Userspace shell not found at $shellPath, skipping initrd" -ForegroundColor Yellow
    }
}

Write-Host "Building Docker test image..." -ForegroundColor Cyan
docker build -t folkering-test -f Dockerfile.test . | Out-Null
if ($LASTEXITCODE -ne 0) {exit 1}

Write-Host "Creating bootable disk image using mtools..." -ForegroundColor Cyan
docker run --rm -v "${PWD}:/test" -w /test ubuntu:22.04 bash -c @"
    set -e
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq && apt-get install -y -qq mtools dosfstools > /dev/null 2>&1

    # Create a 100MB disk image
    dd if=/dev/zero of=boot.img bs=1M count=100 2>/dev/null

    # Format as FAT32
    mkfs.fat -F 32 boot.img > /dev/null

    # Use mtools to copy files without mounting
    export MTOOLS_SKIP_CHECK=1

    # Copy limine.conf
    mcopy -i boot.img iso_root/limine.conf ::/ 2>/dev/null

    # Create boot directory
    mmd -i boot.img ::/boot 2>/dev/null

    # Copy kernel
    mcopy -i boot.img iso_root/boot/kernel.elf ::/boot/ 2>/dev/null

    # Copy initrd if it exists
    if [ -f iso_root/boot/initrd.fpk ]; then
        mcopy -i boot.img iso_root/boot/initrd.fpk ::/boot/ 2>/dev/null
        echo 'initrd.fpk copied to boot image'
    else
        echo 'No initrd.fpk found, booting without ramdisk'
    fi

    # Create and copy limine directory
    mmd -i boot.img ::/boot/limine 2>/dev/null
    mcopy -i boot.img -s iso_root/boot/limine/* ::/boot/limine/ 2>/dev/null || true

    echo 'Disk image created successfully'
"@

if ($LASTEXITCODE -ne 0) {
    Write-Host "Failed to create disk image" -ForegroundColor Red
    exit 1
}

Write-Host ""
Write-Host "Running QEMU boot test..." -ForegroundColor Cyan
Write-Host "Looking for 'HELLO' on VGA and Limine boot messages..." -ForegroundColor Yellow
Write-Host "========================================" -ForegroundColor Yellow
Write-Host ""

# Run QEMU with output capture
docker run --rm `
    -v "${PWD}:/test" `
    -w /test `
    folkering-test `
    -drive file=boot.img,format=raw,if=ide `
    -serial stdio `
    -no-reboot `
    -no-shutdown `
    -m 512M `
    -cpu qemu64 `
    -smp 1 `
    -d cpu_reset,guest_errors 2>&1 | Tee-Object -FilePath "qemu-output.log"

Write-Host ""
Write-Host "========================================" -ForegroundColor Yellow
Write-Host "Test complete" -ForegroundColor Cyan
Write-Host "Output saved to: qemu-output.log" -ForegroundColor Gray
