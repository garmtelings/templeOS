; Boot sector that prints every CPUID leaf the guest sees to the SeaBIOS
; debug port (0x402), one line per leaf/subleaf:
;
;   cpuid LLLLLLLL SSSSSSSS: AAAAAAAA BBBBBBBB CCCCCCCC DDDDDDDD
;
; Used to capture the exact `-cpu qemu64` leaves of the reference machine so
; the VMM can present the same CPU (docs/hw-surface.md §2).
;
; Build: nasm -f bin -o cpuid-dump.img cpuid-dump.asm
; Run:   tools/qemu-ref/cpuid_ref.sh (boots it as a hard disk in QEMU)

bits 16
org 0x7c00

DBG equ 0x402

start:
    cli
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x7c00

    xor esi, esi                ; range base 0x00000000
    call dump_range
    mov esi, 0x40000000
    call dump_range
    mov esi, 0x80000000
    call dump_range

    mov si, done_msg
    call puts
.halt:
    hlt
    jmp .halt

; Dump leaves [esi, max] where max = cpuid(esi).eax, capped at esi+0x3f.
dump_range:
    mov eax, esi
    xor ecx, ecx
    cpuid
    mov edi, eax                ; edi = max leaf
    mov eax, esi
    add eax, 0x3f
    cmp edi, eax
    jbe .ok
    mov edi, eax
.ok:
    ; A max below the base means the range isn't implemented: dump the base
    ; leaf alone so the capture shows what it returns.
    cmp edi, esi
    jae .loop
    mov edi, esi
.loop:
    ; Subleaves 0..3 for the leaves that take one, else just subleaf 0.
    xor ebp, ebp
    mov word [subcnt], 1
    cmp esi, 4
    je .sub
    cmp esi, 7
    je .sub
    cmp esi, 0xb
    je .sub
    cmp esi, 0xd
    je .sub
    cmp esi, 0x8000001d
    je .sub
    jmp .one
.sub:
    mov word [subcnt], 4
.one:
.subloop:
    mov eax, esi
    mov ecx, ebp
    cpuid
    call print_leaf
    inc ebp
    dec word [subcnt]
    jnz .subloop
    cmp esi, edi
    je .done
    inc esi
    jmp .loop
.done:
    ret

; Print "cpuid <esi> <ebp>: <eax> <ebx> <ecx> <edx>\n".
print_leaf:
    push eax
    push ebx
    push ecx
    push edx
    push esi
    mov si, leaf_msg
    call puts
    pop esi
    mov eax, esi
    call hex32
    mov al, ' '
    call putc
    mov eax, ebp
    call hex32
    mov al, ':'
    call putc
    pop edx
    pop ecx
    pop ebx
    pop eax
    push edx
    push ecx
    push ebx
    call space_hex32            ; eax
    pop eax
    call space_hex32            ; ebx
    pop eax
    call space_hex32            ; ecx
    pop eax
    call space_hex32            ; edx
    mov al, 10
    call putc
    ret

space_hex32:
    push eax
    mov al, ' '
    call putc
    pop eax
; Print eax as 8 hex digits. Clobbers eax, ecx, edx.
hex32:
    mov edx, eax
    mov cx, 8
.digit:
    rol edx, 4
    mov al, dl
    and al, 0x0f
    add al, '0'
    cmp al, '9'
    jbe .out
    add al, 'a' - '0' - 10
.out:
    call putc
    dec cx
    jnz .digit
    ret

; Write al to the debug port, preserving dx.
putc:
    push dx
    mov dx, DBG
    out dx, al
    pop dx
    ret

; Print the NUL-terminated string at ds:si.
puts:
    lodsb
    test al, al
    jz .end
    call putc
    jmp puts
.end:
    ret

subcnt dw 0
leaf_msg db "cpuid ", 0
done_msg db "cpuid-dump done", 10, 0

times 510 - ($ - $$) db 0
dw 0xaa55
