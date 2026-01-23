@echo off
REM Test boot script for Folkering OS kernel

cd /d "%~dp0"

echo Building kernel...
cargo build --target x86_64-unknown-none
if %ERRORLEVEL% NEQ 0 (
    echo Build failed!
    pause
    exit /b 1
)

echo Copying kernel to ISO root...
copy /Y target\x86_64-unknown-none\debug\kernel iso_root\boot\kernel.elf

echo Testing boot with QEMU...
qemu-system-x86_64 ^
    -serial stdio ^
    -no-reboot ^
    -no-shutdown ^
    -drive file=iso_root,format=raw,if=virtio ^
    -m 512M ^
    -cpu qemu64 ^
    -smp 1

pause
