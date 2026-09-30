# QEMU reference traces

QEMU is the reference machine: the VMM must behave the same way for
everything TempleOS can observe. These scripts record what TempleOS does to
the hardware when it runs in QEMU.

Requirements: `qemu-system-x86_64` (tested with 8.2.2) and Python 3.

```sh
tools/fetch-payload.sh                      # once: get and verify the ISO
tools/qemu-ref/qemu_trace.py --out ref/boot --seconds 120
tools/qemu-ref/summarize.py --since-kernel ref/boot/trace.log
```

`qemu_trace.py` runs the machine from `docs/hw-surface.md` (i440FX + PIIX3,
HPET, SeaBIOS from `payload/`, the ISO on the secondary IDE master) with
`-icount`, so runs are repeatable. It records:

- `trace.log`: every port and MMIO access with value and size, PCI config
  accesses and IRQ deliveries
- `debugcon.log`: SeaBIOS's debug output
- `shot-NNNN.ppm` and `manifest.json`: screen dumps with their hashes, and
  the exact command line and input hashes

`--iso none` traces the BIOS alone, which is a quick way to check the
tooling works.

## VGA reference runs

`tools/qemu-ref/vga_ref.py` records two short runs that need no ISO:
`ref/vga-text` (SeaBIOS's text screen) and `ref/vga-gfx` (a boot sector
that sets mode 12h through VBE and draws with the map mask, as TempleOS
does). Check the VGA model against them:

```sh
TEMPLEOS_TRACE=ref/vga-text/trace.log cargo test -p devices replay_vga -- --ignored
TEMPLEOS_TRACE=ref/vga-gfx/trace.log cargo test -p devices replay_vga -- --ignored
```

Every read is compared with QEMU's value, and the final frame must equal
QEMU's last screendump pixel for pixel. Without `TEMPLEOS_TRACE` the test
uses `ref/boot/trace.log`.
