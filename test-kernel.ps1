# Folkering OS Kernel Test Script
# Runs QEMU with serial output capture and provides metrics summary

param(
    [int]$Duration = 15,
    [string]$LogFile = "test-output.log",
    [switch]$Interactive
)

$QemuPath = "C:\Program Files\qemu\qemu-system-x86_64.exe"
$BootImage = "boot.img"

if ($Interactive) {
    # Interactive mode - serial to stdout
    Write-Host "Starting Folkering OS in QEMU (interactive)..." -ForegroundColor Green
    Write-Host "Press Ctrl+C to exit" -ForegroundColor Yellow
    Write-Host ""

    & $QemuPath -drive file=$BootImage,format=raw -serial stdio -m 128M -no-reboot
    exit
}

# Automated test mode
Write-Host "=============================================="
Write-Host "  Folkering OS Kernel Test"
Write-Host "=============================================="
Write-Host "Duration: $Duration seconds"
Write-Host ""

# Clean up previous log
if (Test-Path $LogFile) { Remove-Item $LogFile -Force }

# Start QEMU
$proc = Start-Process -FilePath $QemuPath `
    -ArgumentList "-drive","file=$BootImage,format=raw","-serial","file:$LogFile","-m","128M","-no-reboot","-display","none" `
    -NoNewWindow -PassThru

Write-Host "QEMU started (PID: $($proc.Id))"

# Wait with progress
for ($i = 1; $i -le $Duration; $i++) {
    Start-Sleep -Seconds 1
    Write-Host -NoNewline "`rRunning: $i/$Duration sec"
}
Write-Host ""

# Stop QEMU
Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

Write-Host ""
Write-Host "=============================================="
Write-Host "  Results"
Write-Host "=============================================="

if (Test-Path $LogFile) {
    $content = Get-Content $LogFile -Raw

    # Count metrics
    $yields = ([regex]::Matches($content, "YIELD_CPU")).Count
    $ipcRecv = ([regex]::Matches($content, "Got message")).Count
    $switches = ([regex]::Matches($content, "switch_to\(target_id=")).Count
    $pageFaults = ([regex]::Matches($content, "#PF|Page Fault")).Count
    $gpFaults = ([regex]::Matches($content, "#GP")).Count
    $timerOk = $content -match "Timer ENABLED"

    Write-Host ""
    Write-Host "  Yields:        $yields"
    Write-Host "  IPC Messages:  $ipcRecv"
    Write-Host "  Ctx Switches:  $switches"
    Write-Host "  Timer:         $(if($timerOk){'Enabled'}else{'Disabled'})"
    Write-Host ""

    if ($Duration -gt 0) {
        Write-Host "  Yields/sec:    $([math]::Round($yields/$Duration,1))"
        Write-Host "  IPC/sec:       $([math]::Round($ipcRecv/$Duration,1))"
        Write-Host ""
    }

    if ($pageFaults -gt 0 -or $gpFaults -gt 0) {
        Write-Host "ERRORS:" -ForegroundColor Red
        if ($pageFaults -gt 0) { Write-Host "  Page Faults: $pageFaults" -ForegroundColor Red }
        if ($gpFaults -gt 0) { Write-Host "  GP Faults: $gpFaults" -ForegroundColor Red }
        Write-Host ""
        Write-Host "STATUS: FAIL" -ForegroundColor Red
    } elseif ($yields -gt 100) {
        Write-Host "STATUS: PASS" -ForegroundColor Green
    } else {
        Write-Host "STATUS: WARN (low activity)" -ForegroundColor Yellow
    }
} else {
    Write-Host "ERROR: No log file!" -ForegroundColor Red
    Write-Host "STATUS: FAIL" -ForegroundColor Red
}

Write-Host "=============================================="
