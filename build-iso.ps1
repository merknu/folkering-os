# Build ISO for Folkering OS
# PowerShell script to create bootable ISO with Limine bootloader

$ErrorActionPreference = "Stop"

# Configuration
$KERNEL_DIR = $PSScriptRoot
$ISO_ROOT = Join-Path $KERNEL_DIR "iso_root"
$LIMINE_DIR = Join-Path $KERNEL_DIR "limine"
$LIMINE_VERSION = "10.6.3"  # Latest stable version
$KERNEL_BINARY = "target\x86_64-folkering\release\kernel"
$ISO_OUTPUT = "folkering.iso"

Write-Host "==================================" -ForegroundColor Cyan
Write-Host "Folkering OS - ISO Builder" -ForegroundColor Cyan
Write-Host "==================================" -ForegroundColor Cyan
Write-Host ""

# Step 1: Build the kernel if not already built
Write-Host "[1/6] Checking kernel binary..." -ForegroundColor Yellow
if (-not (Test-Path $KERNEL_BINARY)) {
    Write-Host "  Kernel not found. Building..." -ForegroundColor Yellow
    cargo build --release
    if ($LASTEXITCODE -ne 0) {
        Write-Error "Kernel build failed!"
        exit 1
    }
} else {
    Write-Host "  Kernel binary found: $KERNEL_BINARY" -ForegroundColor Green
}

# Step 2: Create ISO directory structure
Write-Host "[2/6] Creating ISO directory structure..." -ForegroundColor Yellow
if (Test-Path $ISO_ROOT) {
    Remove-Item -Recurse -Force $ISO_ROOT
}
New-Item -ItemType Directory -Path $ISO_ROOT | Out-Null
New-Item -ItemType Directory -Path "$ISO_ROOT\boot" | Out-Null
New-Item -ItemType Directory -Path "$ISO_ROOT\boot\limine" | Out-Null
New-Item -ItemType Directory -Path "$ISO_ROOT\EFI" | Out-Null
New-Item -ItemType Directory -Path "$ISO_ROOT\EFI\BOOT" | Out-Null
Write-Host "  Created: $ISO_ROOT" -ForegroundColor Green

# Step 3: Copy kernel binary
Write-Host "[3/6] Copying kernel..." -ForegroundColor Yellow
Copy-Item $KERNEL_BINARY "$ISO_ROOT\kernel"
Write-Host "  Copied: kernel -> iso_root\kernel" -ForegroundColor Green

# Step 4: Download and extract Limine if not present
Write-Host "[4/6] Setting up Limine bootloader..." -ForegroundColor Yellow
if (-not (Test-Path $LIMINE_DIR)) {
    Write-Host "  Downloading Limine v$LIMINE_VERSION..." -ForegroundColor Yellow
    $LIMINE_URL = "https://github.com/limine-bootloader/limine/releases/download/v$LIMINE_VERSION/limine-$LIMINE_VERSION.tar.gz"
    $LIMINE_TARBALL = "limine.tar.gz"

    # Download
    try {
        # Force TLS 1.2 for GitHub
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

        # Download with Invoke-WebRequest
        $ProgressPreference = 'SilentlyContinue'  # Faster downloads
        Invoke-WebRequest -Uri $LIMINE_URL -OutFile $LIMINE_TARBALL -UseBasicParsing
        $ProgressPreference = 'Continue'

        Write-Host "  Downloaded Limine" -ForegroundColor Green
    } catch {
        Write-Error "Failed to download Limine: $_"
        Write-Host ""
        Write-Host "Manual download instructions:" -ForegroundColor Yellow
        Write-Host "1. Download Limine from: $LIMINE_URL" -ForegroundColor Yellow
        Write-Host "2. Extract to: $LIMINE_DIR" -ForegroundColor Yellow
        Write-Host "3. Run this script again" -ForegroundColor Yellow
        exit 1
    }

    # Extract (requires tar on Windows 10+ or WSL)
    Write-Host "  Extracting Limine..." -ForegroundColor Yellow
    tar -xzf $LIMINE_TARBALL
    Move-Item "limine-$LIMINE_VERSION" $LIMINE_DIR
    Remove-Item $LIMINE_TARBALL

    Write-Host "  Limine extracted to: $LIMINE_DIR" -ForegroundColor Green
} else {
    Write-Host "  Limine already present: $LIMINE_DIR" -ForegroundColor Green
}

# Step 5: Deploy Limine binaries
Write-Host "[5/6] Deploying Limine binaries..." -ForegroundColor Yellow

# Copy Limine configuration
Copy-Item "limine.conf" "$ISO_ROOT\boot\limine\limine.conf"
Write-Host "  Copied: limine.conf" -ForegroundColor Green

# Copy Limine binaries (for both BIOS and UEFI)
$LIMINE_FILES = @(
    "limine-bios.sys",
    "limine-bios-cd.bin",
    "limine-uefi-cd.bin"
)

foreach ($file in $LIMINE_FILES) {
    $srcPath = Join-Path $LIMINE_DIR $file
    if (Test-Path $srcPath) {
        Copy-Item $srcPath "$ISO_ROOT\boot\limine\$file"
        Write-Host "  Copied: $file" -ForegroundColor Green
    } else {
        Write-Warning "  Missing: $file (UEFI boot may not work)"
    }
}

# Copy UEFI bootloader
$BOOTX64 = Join-Path $LIMINE_DIR "BOOTX64.EFI"
if (Test-Path $BOOTX64) {
    Copy-Item $BOOTX64 "$ISO_ROOT\EFI\BOOT\BOOTX64.EFI"
    Write-Host "  Copied: BOOTX64.EFI (UEFI support)" -ForegroundColor Green
} else {
    Write-Warning "  Missing: BOOTX64.EFI (UEFI boot will not work)"
}

# Step 6: Create ISO image
Write-Host "[6/6] Creating ISO image..." -ForegroundColor Yellow

# Check if xorriso is available
$XORRISO = Get-Command xorriso -ErrorAction SilentlyContinue
if ($null -eq $XORRISO) {
    Write-Warning "xorriso not found. ISO creation skipped."
    Write-Host ""
    Write-Host "To create the ISO, install xorriso and run:" -ForegroundColor Yellow
    Write-Host "  xorriso -as mkisofs -b boot/limine/limine-bios-cd.bin \\" -ForegroundColor Cyan
    Write-Host "    -no-emul-boot -boot-load-size 4 -boot-info-table \\" -ForegroundColor Cyan
    Write-Host "    --efi-boot boot/limine/limine-uefi-cd.bin \\" -ForegroundColor Cyan
    Write-Host "    -efi-boot-part --efi-boot-image --protective-msdos-label \\" -ForegroundColor Cyan
    Write-Host "    iso_root -o $ISO_OUTPUT" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "Alternatively, test directly with QEMU:" -ForegroundColor Yellow
    Write-Host "  qemu-system-x86_64 -kernel $KERNEL_BINARY -serial stdio" -ForegroundColor Cyan
} else {
    # Create ISO with xorriso
    $XORRISO_ARGS = @(
        "-as", "mkisofs",
        "-b", "boot/limine/limine-bios-cd.bin",
        "-no-emul-boot",
        "-boot-load-size", "4",
        "-boot-info-table",
        "--efi-boot", "boot/limine/limine-uefi-cd.bin",
        "-efi-boot-part",
        "--efi-boot-image",
        "--protective-msdos-label",
        $ISO_ROOT,
        "-o", $ISO_OUTPUT
    )

    & xorriso @XORRISO_ARGS

    if ($LASTEXITCODE -eq 0) {
        Write-Host "  ISO created: $ISO_OUTPUT" -ForegroundColor Green

        # Get file size
        $isoSize = (Get-Item $ISO_OUTPUT).Length / 1MB
        Write-Host "  Size: $([math]::Round($isoSize, 2)) MB" -ForegroundColor Green
    } else {
        Write-Error "ISO creation failed!"
        exit 1
    }
}

Write-Host ""
Write-Host "==================================" -ForegroundColor Green
Write-Host "Build Complete!" -ForegroundColor Green
Write-Host "==================================" -ForegroundColor Green
Write-Host ""

if (Test-Path $ISO_OUTPUT) {
    Write-Host "Next steps:" -ForegroundColor Yellow
    Write-Host "1. Test in QEMU:" -ForegroundColor Cyan
    Write-Host "     qemu-system-x86_64 -cdrom $ISO_OUTPUT -serial stdio" -ForegroundColor White
    Write-Host ""
    Write-Host "2. Test with more memory:" -ForegroundColor Cyan
    Write-Host "     qemu-system-x86_64 -cdrom $ISO_OUTPUT -m 512M -serial stdio" -ForegroundColor White
    Write-Host ""
    Write-Host "3. Test with UEFI:" -ForegroundColor Cyan
    Write-Host "     qemu-system-x86_64 -bios OVMF.fd -cdrom $ISO_OUTPUT -serial stdio" -ForegroundColor White
} else {
    Write-Host "ISO creation skipped. Directory prepared at: $ISO_ROOT" -ForegroundColor Yellow
}
