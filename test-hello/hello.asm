; Simple Hello World kernel for testing Limine boot
; Writes directly to COM1 serial port (0x3F8)

bits 64
section .text

global _start
_start:
    ; Initialize serial port COM1 (0x3F8)
    mov dx, 0x3F8 + 3        ; Line Control Register
    mov al, 0x80             ; Enable DLAB
    out dx, al

    mov dx, 0x3F8 + 0        ; Divisor Latch Low
    mov al, 0x03             ; Divisor = 3 (38400 baud)
    out dx, al

    mov dx, 0x3F8 + 1        ; Divisor Latch High
    mov al, 0x00
    out dx, al

    mov dx, 0x3F8 + 3        ; Line Control Register
    mov al, 0x03             ; 8 bits, no parity, 1 stop bit
    out dx, al

    mov dx, 0x3F8 + 2        ; FIFO Control Register
    mov al, 0xC7             ; Enable FIFO
    out dx, al

    mov dx, 0x3F8 + 4        ; Modem Control Register
    mov al, 0x0B             ; RTS/DSR set
    out dx, al

    ; Write "HELLO FROM TEST KERNEL!\n"
    mov rsi, hello_msg
    mov rcx, hello_msg_len
.write_loop:
    lodsb                    ; Load byte from [rsi] into al
    mov dx, 0x3F8            ; Data register
    out dx, al               ; Write character
    loop .write_loop

    ; Halt
.hang:
    hlt
    jmp .hang

section .rodata
hello_msg:
    db "HELLO FROM TEST KERNEL!", 0x0A
hello_msg_len equ $ - hello_msg
