#!/bin/bash
# Siste forsøk: Test om QEMU finnes lokalt i Git Bash

echo "=== SISTE FORSØK: LOKAL QEMU TEST ==="
echo ""

# Sjekk om QEMU finnes i PATH eller vanlige steder
QEMU_PATHS=(
    "qemu-system-x86_64"
    "/c/Program Files/qemu/qemu-system-x86_64.exe"
    "/c/Program Files (x86)/qemu/qemu-system-x86_64.exe"
    "/usr/bin/qemu-system-x86_64"
    "/mingw64/bin/qemu-system-x86_64.exe"
)

QEMU_FOUND=""
for path in "${QEMU_PATHS[@]}"; do
    if [ -f "$path" ] || command -v "$path" &> /dev/null; then
        echo "✓ Fant QEMU: $path"
        QEMU_FOUND="$path"
        break
    fi
done

if [ -z "$QEMU_FOUND" ]; then
    echo "✗ Ingen lokal QEMU installasjon funnet"
    echo ""
    echo "Søkte i:"
    for path in "${QEMU_PATHS[@]}"; do
        echo "  - $path"
    done
    echo ""
    echo "LØSNING:"
    echo "1. Last ned: https://qemu.weilnetz.de/w64/"
    echo "2. Installer til: C:\Program Files\qemu\\"
    echo "3. Kjør dette scriptet på nytt"
    exit 1
fi

echo ""
echo "Testing QEMU versjon..."
"$QEMU_FOUND" --version | head -2
echo ""

echo "Booting Folkering OS med lokal QEMU..."
echo "Timeout: 30 sekunder"
echo ""

rm -f BOOT-SUCCESS.log

timeout 30 "$QEMU_FOUND" \
    -drive file=working-boot.img,format=raw,if=ide \
    -serial file:BOOT-SUCCESS.log \
    -m 512M \
    -display none \
    -no-reboot \
    -no-shutdown \
    2>&1 || echo "QEMU avsluttet"

echo ""
echo "=== RESULTAT ==="
if [ -f BOOT-SUCCESS.log ]; then
    SIZE=$(stat -c%s BOOT-SUCCESS.log 2>/dev/null || stat -f%z BOOT-SUCCESS.log 2>/dev/null || echo "0")
    echo "Log fil: BOOT-SUCCESS.log ($SIZE bytes)"
    
    if [ "$SIZE" -gt "0" ]; then
        echo ""
        echo "✓✓✓ SUCCESS! Output mottatt! ✓✓✓"
        echo ""
        echo "=== BOOT OUTPUT ==="
        cat BOOT-SUCCESS.log
        echo ""
        echo "=== SLUTT ==="
        
        # Sjekk om IPC test passerte
        if grep -q "IPC.*PASSED" BOOT-SUCCESS.log; then
            echo ""
            echo "🎉🎉🎉 IPC TEST PASSERTE! 🎉🎉🎉"
            echo ""
            echo "Option B er 100% funksjonell!"
        fi
    else
        echo "✗ Filen er tom (0 bytes)"
        echo ""
        echo "Dette betyr fortsatt Docker/Windows I/O problem."
        echo "Prøv å kjøre QEMU direkte i PowerShell:"
        echo ""
        echo '  & "C:\Program Files\qemu\qemu-system-x86_64.exe" \'
        echo '    -drive file=working-boot.img,format=raw,if=ide \'
        echo '    -serial file:BOOT-SUCCESS.log \'
        echo '    -m 512M \'
        echo '    -display none \'
        echo '    -no-reboot'
    fi
else
    echo "✗ Ingen log fil opprettet"
fi
