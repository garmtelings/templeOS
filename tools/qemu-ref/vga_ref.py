#!/usr/bin/env python3
"""Record the two small VGA reference runs the VGA model is checked against.

Neither needs the TempleOS ISO:

  ref/vga-text/  SeaBIOS alone. SeaVGABIOS loads its fonts and sets text
                 mode 3, and SeaBIOS prints "No bootable device": font
                 loading through planar memory, odd/even text writes.
  ref/vga-gfx/   A boot sector doing what TempleOS's KStart16 and
                 GrUpdateVGAGraphics do: VBE 4F02h with mode 0x12, then
                 writes through the map mask to two planes (REP STOSB) and
                 a DAC palette change.

Then check the model against each:

  TEMPLEOS_TRACE=ref/vga-text/trace.log cargo test -p devices replay_vga -- --ignored
  TEMPLEOS_TRACE=ref/vga-gfx/trace.log cargo test -p devices replay_vga -- --ignored

The test replays every VGA access, compares every read, and requires the
final frame to equal QEMU's last screendump pixel for pixel.
"""

import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))

# 16-bit real-mode code, hand assembled:
#   mov ax,4F02h / mov bx,12h / int 10h        VBE set mode 12h
#   mov ax,A000h / mov es,ax / mov dx,3C4h
#   mov ax,0102h / out dx,ax                   map mask = plane 0
#   xor di,di / mov al,F0h / mov cx,4000 / rep stosb
#   mov ax,0402h / out dx,ax                   map mask = plane 2
#   mov di,2000 / mov al,3Ch / mov cx,8000 / rep stosb
#   mov ax,0F02h / out dx,ax                   all planes
#   mov dx,3C8h / mov al,1 / out dx,al / inc dx
#   mov al,3Fh / out / mov al,15h / out / mov al,2Ah / out   DAC entry 1
#   cli / hlt / jmp $
GFX_CODE = bytes.fromhex(
    "B8024F BB1200 CD10 B800A0 8EC0 BAC403 B80201 EF 31FF B0F0 B9A00F F3AA"
    " B80204 EF BFD007 B03C B9401F F3AA B8020F EF BAC803 B001 EE 42 B03F EE"
    " B015 EE B02A EE FAF4 EBFE".replace(" ", "")
)


def main():
    out = os.path.join(ROOT, "ref")
    os.makedirs(out, exist_ok=True)
    img = os.path.join(out, "vga-gfx.img")
    disk = bytearray(1 << 20)
    disk[: len(GFX_CODE)] = GFX_CODE
    disk[510:512] = b"\x55\xaa"
    with open(img, "wb") as f:
        f.write(disk)

    trace = os.path.join(HERE, "qemu_trace.py")
    common = ["--iso", "none", "--seconds", "6", "--shot-every", "3"]
    for name, extra in [("vga-text", []), ("vga-gfx", ["--hdd", img])]:
        subprocess.run([sys.executable, trace, *common, *extra,
                        "--out", os.path.join(out, name)], check=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
