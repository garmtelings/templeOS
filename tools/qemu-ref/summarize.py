#!/usr/bin/env python3
"""Summarize a QEMU reference trace into the hardware surface it exercises.

Reads trace.log written by qemu_trace.py and prints, per device region:
which I/O ports / MMIO offsets were touched, in which direction, with which
access sizes, and how often. With --since-kernel, accesses before the kernel's
TimersInit() are dropped, so what's left is the running OS's own surface
without SeaBIOS's POST (the few accesses KStart16/KMain make before
TimersInit are listed by hand in docs/hw-surface.md).

Output is Markdown, so it can be pasted into docs/hw-surface.md.
"""

import argparse
import collections
import re
import sys

LINE = re.compile(
    r"memory_region_ops_(?P<dir>read|write) cpu (?P<cpu>-?\d+) mr \S+ "
    r"addr (?P<addr>0x[0-9a-f]+) value (?P<val>0x[0-9a-f]+) "
    r"size (?P<size>\d+) name '(?P<name>[^']*)'")
PCI = re.compile(r"pci_cfg_(?P<dir>read|write) (?P<dev>\S+) (?P<bdf>[0-9a-f:.]+) "
                 r"@(?P<reg>0x[0-9a-f]+) (?:->|<-) (?P<val>0x[0-9a-f]+)")

# We key on the first write that only TempleOS makes: TimersInit() loading
# PIT channel 0 with SYS_TIMER0_PERIOD = 0x04A8 (low byte 0xA8 to port 0x40).
# SeaBIOS also sends 0x34 to port 0x43, but then loads a count of 0.
KERNEL_MARKER = ("write", 0x40, 0xA8)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("trace")
    ap.add_argument("--since-kernel", action="store_true")
    args = ap.parse_args()

    ports = collections.defaultdict(lambda: collections.Counter())
    mmio = collections.defaultdict(lambda: collections.Counter())
    names = {}
    pci = collections.Counter()
    started = not args.since_kernel
    marker_seen = False

    with open(args.trace, errors="replace") as f:
        for line in f:
            m = LINE.search(line)
            if m:
                d, addr, size = m["dir"], int(m["addr"], 16), int(m["size"])
                if not started:
                    if (d, addr, int(m["val"], 16)) == KERNEL_MARKER:
                        started = marker_seen = True
                    else:
                        continue
                name = m["name"]
                if addr < 0x10000:
                    ports[addr][(d, size)] += 1
                    names[("io", addr)] = name
                else:
                    page = addr & ~0xFFF
                    mmio[(name, page)][(d, size, addr & 0xFFF)] += 1
                continue
            m = PCI.search(line)
            if m and started:
                pci[(m["dir"], m["bdf"], int(m["reg"], 16))] += 1

    if args.since_kernel and not marker_seen:
        print("warning: kernel marker (PIT reprogram) never seen; "
              "did the kernel boot?", file=sys.stderr)

    print("## Port I/O\n")
    print("| Port | Region | Accesses (dir/size: count) |")
    print("|---|---|---|")
    for port in sorted(ports):
        acc = ", ".join(f"{d[0].upper()}{s * 8}: {n}"
                        for (d, s), n in sorted(ports[port].items()))
        print(f"| 0x{port:04X} | {names[('io', port)]} | {acc} |")

    print("\n## MMIO\n")
    print("| Region | Page | Distinct offsets | Reads | Writes | Sizes |")
    print("|---|---|---|---|---|---|")
    for (name, page), c in sorted(mmio.items(), key=lambda kv: kv[0][1]):
        offs = {o for (_, _, o) in c}
        reads = sum(n for (d, _, _), n in c.items() if d == "read")
        writes = sum(n for (d, _, _), n in c.items() if d == "write")
        sizes = sorted({s for (_, s, _) in c})
        print(f"| {name} | 0x{page:08X} | {len(offs)} | {reads} | {writes} | "
              f"{','.join(map(str, sizes))} |")

    print("\n## PCI config space\n")
    print("| Dir | BDF | Reg | Count |")
    print("|---|---|---|---|")
    for (d, bdf, reg), n in sorted(pci.items()):
        print(f"| {d} | {bdf} | 0x{reg:02X} | {n} |")
    return 0


if __name__ == "__main__":
    sys.exit(main())
