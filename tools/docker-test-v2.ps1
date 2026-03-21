# Docker-based boot test for Folkering OS kernel (Windows)
# Run from project root: folkering-os/
# Uses mtools instead of mount

$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot

Write-Host "Project root: $ProjectRoot" -ForegroundColor Gray
Set-Location $ProjectRoot

# Step 1: Build kernel (must run from kernel/ for .cargo/config.toml)
Write-Host "Building kernel..." -ForegroundColor Cyan
Push-Location kernel
cargo build
if ($LASTEXITCODE -ne 0) { Pop-Location; exit 1 }
Pop-Location

Write-Host "Copying kernel to ISO root..." -ForegroundColor Cyan
Copy-Item -Force "kernel\target\x86_64-folkering\debug\kernel" "boot\iso_root\boot\kernel.elf"

# Step 2: Build folk-pack tool and create initrd
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
        & $folkPackExe create "boot\iso_root\boot\initrd.fpk" --add "shell:elf:$shellPath" --add "hello.txt:data:boot\hello.txt"
        if ($LASTEXITCODE -ne 0) {
            Write-Host "folk-pack create failed, continuing without initrd" -ForegroundColor Yellow
        } else {
            Write-Host "initrd.fpk created successfully" -ForegroundColor Green
        }
    } else {
        Write-Host "Userspace shell not found at $shellPath, skipping initrd" -ForegroundColor Yellow
    }
}

# Step 3: Build Docker test image
Write-Host "Building Docker test image..." -ForegroundColor Cyan
docker build -t folkering-test -f tools\Dockerfile.test tools\ | Out-Null
if ($LASTEXITCODE -ne 0) { exit 1 }

# Step 4: Create bootable disk image using mtools in Docker
Write-Host "Creating bootable disk image using mtools..." -ForegroundColor Cyan
docker run --rm -v "${ProjectRoot}:/project" -w /project ubuntu:22.04 bash -c @"
    set -e
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq && apt-get install -y -qq mtools dosfstools > /dev/null 2>&1

    # Create a 100MB disk image
    dd if=/dev/zero of=boot/boot.img bs=1M count=100 2>/dev/null

    # Format as FAT32
    mkfs.fat -F 32 boot/boot.img > /dev/null

    # Use mtools to copy files without mounting
    export MTOOLS_SKIP_CHECK=1

    # Copy limine.conf to root (Limine looks for it at /)
    mcopy -i boot/boot.img boot/limine.conf ::/ 2>/dev/null

    # Create boot directory
    mmd -i boot/boot.img ::/boot 2>/dev/null

    # Copy kernel
    mcopy -i boot/boot.img boot/iso_root/boot/kernel.elf ::/boot/ 2>/dev/null

    # Copy initrd if it exists
    if [ -f boot/iso_root/boot/initrd.fpk ]; then
        mcopy -i boot/boot.img boot/iso_root/boot/initrd.fpk ::/boot/ 2>/dev/null
        echo 'initrd.fpk copied to boot image'
    else
        echo 'No initrd.fpk found, booting without ramdisk'
    fi

    # Copy limine bootloader files
    mmd -i boot/boot.img ::/boot/limine 2>/dev/null
    if [ -f boot/limine-bios.sys ]; then
        mcopy -i boot/boot.img boot/limine-bios.sys ::/boot/limine/ 2>/dev/null
    fi
    if [ -d boot/iso_root/boot/limine ]; then
        mcopy -i boot/boot.img -s boot/iso_root/boot/limine/* ::/boot/limine/ 2>/dev/null || true
    fi

    # Install Limine MBR if available
    if [ -f boot/limine/bin/limine ]; then
        boot/limine/bin/limine bios-install boot/boot.img 2>/dev/null && echo 'Limine MBR installed' || echo 'Limine MBR install skipped'
    fi

    echo 'Disk image created successfully'
"@

if ($LASTEXITCODE -ne 0) {
    Write-Host "Failed to create disk image" -ForegroundColor Red
    exit 1
}

# Step 5: Run QEMU boot test
Write-Host ""
Write-Host "Running QEMU boot test..." -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Yellow
Write-Host ""

$ErrorActionPreference = "Continue"
docker run --rm `
    -v "${ProjectRoot}:/project" `
    -w /project `
    folkering-test `
    -drive file=boot/boot.img,format=raw,if=ide `
    -serial stdio `
    -display none `
    -no-reboot `
    -no-shutdown `
    -m 512M `
    -cpu qemu64 `
    -smp 1 `
    -d cpu_reset,guest_errors 2>&1 | Tee-Object -FilePath "boot\qemu-output.log"
$ErrorActionPreference = "Stop"

Write-Host ""
Write-Host "========================================" -ForegroundColor Yellow
Write-Host "Test complete" -ForegroundColor Cyan
Write-Host "Output saved to: boot\qemu-output.log" -ForegroundColor Gray
