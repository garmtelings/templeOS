; ATA probe: a boot disk that drives the primary IDE master the way SeaBIOS
; and TempleOS do, so a QEMU trace of it pins down the hard disk model
; (devices/src/ide.rs). Build: nasm -f bin ata_probe.asm -o ata_probe.bin
; tools/qemu-ref/disk_ref.py builds the disk image around it and records
; the reference run.
;
; Stage 1 (the MBR) loads stage 2 with INT 13h AH=42h (so SeaBIOS's own disk
; reads are in the trace), then stage 2 issues ATA commands directly and
; reads back the task file after each one. Every register read lands in the
; trace, where the replay test compares it with the model.

bits 16
org 0x7c00

DATA    equ 0x1f0
CTL     equ 0x3f6
DBG     equ 0x402
STAGE2_SECTORS equ 8

stage1:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7c00
    sti
    mov [drive], dl
    mov si, dap
    mov ah, 0x42
    int 0x13
    jc fail1
    jmp stage2
fail1:
    mov al, 'E'
    mov dx, DBG
    out dx, al
    cli
    hlt

drive: db 0
align 4
dap:
    db 0x10, 0
    dw STAGE2_SECTORS
    dw 0x7e00, 0
    dq 1

    times 446 - ($ - $$) db 0
    ; One partition entry ending at head 15, sector 63, so QEMU's
    ; geometry guess (guess_disk_lchs) finds a logical geometry.
    db 0x80, 1, 1, 0, 0x83, 15, 0xff, 0xff
    dd 63
    dd 65536 - 63
    times 510 - ($ - $$) db 0
    dw 0xaa55

; ---------------------------------------------------------------------------
stage2:
    mov al, 'S'
    mov dx, DBG
    out dx, al

    ; TempleOS ATABlkSel: control = 0x08 (bit 3; nIEN clear).
    mov dx, CTL
    mov al, 0x08
    out dx, al

    ; IDENTIFY DEVICE, LBA select as TempleOS (0xE0 | unit << 4).
    mov al, 0xe0
    call select
    mov al, 0xec
    call command
    call wait_drq
    mov cx, 256
    mov di, buffer
    mov dx, DATA
    rep insw
    call dump

    ; READ NATIVE MAX EXT, read low then HOB bytes (ATAReadNativeMax).
    mov al, 0xef
    call select
    mov al, 0x27
    call command
    call wait_not_busy
    call dump
    mov dx, CTL
    mov al, 0x80
    out dx, al
    call dump
    mov dx, CTL
    mov al, 0x08
    out dx, al

    ; READ NATIVE MAX (LBA28), SET MAX EXT and SET MAX (QEMU aborts both).
    mov al, 0xe0
    call select
    mov al, 0xf8
    call command
    call wait_not_busy
    call dump
    mov al, 0x37
    call command
    call wait_not_busy
    call dump
    mov al, 0xf9
    call command
    call wait_not_busy
    call dump

    ; Fill the buffer with a pattern for the writes.
    mov di, buffer
    mov cx, 512 * 3
    xor al, al
.fill:
    stosb
    add al, 7
    loop .fill

    ; WRITE MULTIPLE EXT: LBA 200, 3 sectors, one sector per DRQ poll
    ; with 32-bit OUTs, exactly as TempleOS's ATAWriteBlks.
    mov eax, 200
    mov bx, 3
    call blksel48
    mov al, 0x39
    call command
    mov si, buffer
    mov bp, 3
.wsec:
    call wait_drdy_drq
    mov cx, 128
    mov dx, DATA
    rep outsd
    dec bp
    jnz .wsec
    call wait_not_busy
    call dump

    ; READ MULTIPLE EXT: LBA 198, 20 sectors (crosses the 16-sector block),
    ; 512 bytes per DRQ with 32-bit INs (ATAGetRes).
    mov eax, 198
    mov bx, 20
    call blksel48
    mov al, 0x29
    call command
    mov bp, 20
.rsec:
    call wait_drq
    mov cx, 128
    mov di, buffer
    mov dx, DATA
    rep insd
    dec bp
    jnz .rsec
    call wait_not_busy
    call dump

    ; READ SECTORS (LBA28): LBA 1, 2 sectors (stage 2 itself).
    mov eax, 1
    mov bx, 2
    call blksel28
    mov al, 0x20
    call command
    mov bp, 2
.r28:
    call wait_drq
    mov cx, 256
    mov di, buffer
    mov dx, DATA
    rep insw
    dec bp
    jnz .r28
    call dump

    ; WRITE SECTORS (LBA28): LBA 210, 2 sectors.
    mov eax, 210
    mov bx, 2
    call blksel28
    mov al, 0x30
    call command
    mov si, buffer
    mov bp, 2
.w28:
    call wait_drq
    mov cx, 256
    mov dx, DATA
    rep outsw
    dec bp
    jnz .w28
    call wait_not_busy
    call dump

    ; SET MULTIPLE MODE 4, READ MULTIPLE LBA 196, 9 sectors (4 + 4 + 1).
    mov dx, DATA + 2
    mov al, 4
    out dx, al
    mov al, 0xc6
    call command
    call wait_not_busy
    call dump
    mov eax, 196
    mov bx, 9
    call blksel28
    mov al, 0xc4
    call command
    mov bp, 9
.rm:
    call wait_drq
    mov cx, 256
    mov di, buffer
    mov dx, DATA
    rep insw
    dec bp
    jnz .rm
    call dump

    ; SET MULTIPLE MODE 3: not a power of two, aborted.
    mov dx, DATA + 2
    mov al, 3
    out dx, al
    mov al, 0xc6
    call command
    call wait_not_busy
    call dump

    ; READ VERIFY, SEEK, FLUSH CACHE (+EXT), CHECK POWER MODE, NOP, DEVICE
    ; RESET (the last two abort on a disk), RECALIBRATE, IDLE IMMEDIATE.
    mov eax, 100
    mov bx, 4
    call blksel28
    mov si, misc_cmds
.misc:
    lodsb
    test al, al
    jz .misc_done
    call command
    call wait_not_busy
    call dump
    jmp .misc
.misc_done:

    ; INITIALIZE DEVICE PARAMETERS (8 heads, 32 sectors), then a CHS read of
    ; cylinder 1, head 2, sector 3 through that geometry.
    mov dx, DATA + 6
    mov al, 0xa7
    out dx, al
    mov dx, DATA + 2
    mov al, 32
    out dx, al
    mov al, 0x91
    call command
    call wait_not_busy
    call dump
    mov dx, DATA + 2
    mov al, 1
    out dx, al
    inc dx
    mov al, 3
    out dx, al
    inc dx
    mov al, 1
    out dx, al
    inc dx
    xor al, al
    out dx, al
    inc dx
    mov al, 0xa2
    out dx, al
    mov al, 0x20
    call command
    call wait_drq
    mov cx, 256
    mov di, buffer
    mov dx, DATA
    rep insw
    call dump

    ; READ SECTORS past the end of the disk (65535 + 2): error.
    mov eax, 65535
    mov bx, 2
    call blksel28
    mov al, 0x20
    call command
    call wait_not_busy
    call dump

    ; Software reset: disk signature back in the task file.
    mov dx, CTL
    mov al, 0x0c
    out dx, al
    mov al, 0x08
    out dx, al
    call wait_not_busy
    call dump

    mov al, 'D'
    mov dx, DBG
    out dx, al
    cli
.halt:
    hlt
    jmp .halt

; ---------------------------------------------------------------------------
; al = device/head value
select:
    mov dx, DATA + 6
    out dx, al
    ret

; al = command; features 0 first, as TempleOS's ATACmd
command:
    push ax
    mov dx, DATA + 1
    xor al, al
    out dx, al
    pop ax
    mov dx, DATA + 7
    out dx, al
    ret

wait_not_busy:
    mov dx, DATA + 7
.l:
    in al, dx
    test al, 0x80
    jnz .l
    ret

wait_drq:
    mov dx, DATA + 7
.l:
    in al, dx
    test al, 0x80
    jnz .l
    test al, 0x09          ; DRQ or ERR
    jz .l
    ret

wait_drdy_drq:
    mov dx, DATA + 7
.l:
    in al, dx
    and al, 0x48
    cmp al, 0x48
    jne .l
    ret

; eax = LBA, bx = count (TempleOS ATABlkSel with BDF_EXT_SIZE)
blksel48:
    push eax
    mov dx, CTL
    mov al, 0x08
    out dx, al
    pop eax
    mov dx, DATA + 2
    mov cl, al
    mov al, bh
    out dx, al             ; count high
    mov dx, DATA + 3
    ror eax, 24
    out dx, al             ; LBA 24-31
    rol eax, 24
    inc dx
    xor al, al
    out dx, al             ; LBA 32-39
    inc dx
    out dx, al             ; LBA 40-47
    mov dx, DATA + 2
    mov al, bl
    out dx, al             ; count low
    inc dx
    mov al, cl
    out dx, al             ; LBA 0-7
    inc dx
    ror eax, 8
    out dx, al             ; LBA 8-15
    inc dx
    ror eax, 8
    out dx, al             ; LBA 16-23
    mov al, 0xef
    call select
    ret

; eax = LBA, bx = count (LBA28)
blksel28:
    mov dx, DATA + 2
    mov cl, al
    mov al, bl
    out dx, al
    inc dx
    mov al, cl
    out dx, al
    inc dx
    ror eax, 8
    out dx, al
    inc dx
    ror eax, 8
    out dx, al
    ror eax, 8
    and al, 0x0f
    or al, 0xe0
    call select
    ret

; Read error, count, LBA, device, alternate status and status.
dump:
    push ax
    push cx
    mov dx, DATA + 1
    mov cx, 6
.l:
    in al, dx
    inc dx
    loop .l
    mov dx, CTL
    in al, dx
    mov dx, DATA + 7
    in al, dx
    pop cx
    pop ax
    ret

misc_cmds: db 0x40, 0x70, 0xe7, 0xea, 0xe5, 0x00, 0x08, 0x10, 0xe1, 0

    times 512 * (1 + STAGE2_SECTORS) - ($ - $$) db 0

buffer equ 0x9000
