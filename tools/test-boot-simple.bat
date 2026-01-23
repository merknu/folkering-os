@echo off
cd /d "%~dp0"

echo Testing kernel boot with QEMU in Docker...
echo =========================================
echo.

docker run --rm -v "%CD%:/test" -w /test folkering-test -drive file=boot.img,format=raw,if=ide -serial stdio -no-reboot -no-shutdown -m 512M -cpu qemu64 -smp 1 -display none

echo.
echo =========================================
echo Test complete
pause
