#!/usr/bin/env python3
"""Compare two directories of screenshots (PPM, as templeos.exe --script and
tools/qemu-ref/qemu_trace.py --script write them) pixel for pixel.

  tools/compare_shots.py ours/ qemu/ [--mask Y0:Y1 ...] [--masks FILE]

For each name present in both: identical, or the number of differing pixels
and their bounding box. --mask ignores rows Y0..Y1-1 (e.g. TempleOS's top
line, whose clock and CPU meters depend on timing: --mask 0:8). --masks
reads rectangles to ignore, one per line: `SHOT X0 Y0 X1 Y1` (ends
exclusive; SHOT `*` means every screenshot; `#` starts a comment). Use it
for what the guest draws from the time: scrolling window titles, the
cursor blink, the shell's command timings. Exits 1 if any screenshot
differs or is missing on one side.
"""

import argparse
import os
import sys


def read_ppm(path):
    data = open(path, "rb").read()
    parts, pos = [], 0
    while len(parts) < 4:
        while data[pos:pos + 1].isspace():
            pos += 1
        if data[pos:pos + 1] == b"#":
            pos = data.index(b"\n", pos) + 1
            continue
        end = pos
        while not data[end:end + 1].isspace():
            end += 1
        parts.append(data[pos:end])
        pos = end
    if parts[0] != b"P6" or parts[3] != b"255":
        raise ValueError(f"{path}: not an 8-bit P6 PPM")
    w, h = int(parts[1]), int(parts[2])
    return w, h, data[pos + 1:pos + 1 + w * h * 3]


def read_masks(path):
    rects = []
    for n, line in enumerate(open(path), 1):
        fields = line.split("#", 1)[0].split()
        if not fields:
            continue
        if len(fields) != 5:
            raise ValueError(f"{path}:{n}: expected SHOT X0 Y0 X1 Y1")
        rects.append((fields[0], *(int(v) for v in fields[1:])))
    return rects


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("a")
    ap.add_argument("b")
    ap.add_argument("--mask", action="append", default=[], help="Y0:Y1 rows to ignore")
    ap.add_argument("--masks", help="file of `SHOT X0 Y0 X1 Y1` rectangles to ignore")
    args = ap.parse_args()
    masks = [tuple(int(v) for v in m.split(":")) for m in args.mask]
    rects = read_masks(args.masks) if args.masks else []

    names_a = {f[:-4] for f in os.listdir(args.a) if f.endswith(".ppm")}
    names_b = {f[:-4] for f in os.listdir(args.b) if f.endswith(".ppm")}
    bad = False
    for name in sorted(names_a | names_b):
        if name not in names_a or name not in names_b:
            print(f"{name}: only in {args.a if name in names_a else args.b}")
            bad = True
            continue
        wa, ha, pa = read_ppm(os.path.join(args.a, name + ".ppm"))
        wb, hb, pb = read_ppm(os.path.join(args.b, name + ".ppm"))
        if (wa, ha) != (wb, hb):
            print(f"{name}: size {wa}x{ha} vs {wb}x{hb}")
            bad = True
            continue
        shot_rects = [r[1:] for r in rects if r[0] in ("*", name)]
        diff, box = 0, None
        for y in range(ha):
            if any(y0 <= y < y1 for y0, y1 in masks):
                continue
            row = y * wa * 3
            if pa[row:row + wa * 3] == pb[row:row + wa * 3]:
                continue
            for x in range(wa):
                i = row + x * 3
                if any(x0 <= x < x1 and y0 <= y < y1 for x0, y0, x1, y1 in shot_rects):
                    continue
                if pa[i:i + 3] != pb[i:i + 3]:
                    diff += 1
                    box = (x, y, x, y) if box is None else (
                        min(box[0], x), min(box[1], y), max(box[2], x), max(box[3], y))
        if diff:
            bad = True
            print(f"{name}: {diff} pixels differ, in x {box[0]}-{box[2]}, y {box[1]}-{box[3]}")
        else:
            print(f"{name}: identical ({wa}x{ha})")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
