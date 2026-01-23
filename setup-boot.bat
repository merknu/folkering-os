@echo off
cd /d "%~dp0"

echo Copying files to boot disk with Docker...

docker run --rm -v "%CD%:/work" -w /work ubuntu:22.04 bash -c "apt-get update -qq && apt-get install -y -qq mtools > /dev/null 2>&1 && export MTOOLS_SKIP_CHECK=1 && mcopy -i /tmp/boot.img limine.conf :: && mmd -i /tmp/boot.img ::/boot 2>/dev/null && mcopy -i /tmp/boot.img iso_root/boot/kernel.elf ::/boot/ && mcopy -i /tmp/boot.img iso_root/boot/limine-bios.sys ::/boot/ && echo Files copied successfully && mdir -i /tmp/boot.img :: && mdir -i /tmp/boot.img ::/boot"

if %ERRORLEVEL% NEQ 0 (
    echo Failed to copy files
    pause
    exit /b 1
)

echo.
echo Boot disk setup complete!
echo.
pause
