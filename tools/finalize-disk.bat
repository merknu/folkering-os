@echo off
cd /d "%~dp0"

echo Finalizing boot disk with Docker...
echo.

docker run --rm -v "%CD%:/work" -w /work ubuntu:22.04 bash /work/finalize-boot-disk.sh

if %ERRORLEVEL% EQU 0 (
    echo.
    echo ========================================
    echo Boot disk ready!
    echo ========================================
) else (
    echo.
    echo Failed to finalize boot disk
)

pause
