#!/usr/bin/env python3
"""Show where two builds of a PE file differ: by section, with the first
differing offsets, the debug directory (PDB name, GUID) and any differing
printable strings. For finding why a build isn't reproducible.

  tools/pe_diff.py A.exe B.exe
"""

import re
import struct
import sys


def sections(d):
    pe = struct.unpack_from("<I", d, 0x3C)[0]
    n = struct.unpack_from("<H", d, pe + 6)[0]
    opt_size = struct.unpack_from("<H", d, pe + 20)[0]
    out = [("headers", 0, struct.unpack_from("<I", d, pe + 24 + 60)[0], 0)]
    for i in range(n):
        s = pe + 24 + opt_size + 40 * i
        name = d[s:s + 8].rstrip(b"\0").decode()
        vsize, va, rsize, raw = struct.unpack_from("<IIII", d, s + 8)
        out.append((name, raw, rsize, va))
    return out


def codeview(d):
    i = d.find(b"RSDS")
    if i < 0:
        return None
    guid = d[i + 4:i + 20].hex()
    age = struct.unpack_from("<I", d, i + 20)[0]
    name = d[i + 24:d.index(b"\0", i + 24)].decode(errors="replace")
    return f"RSDS guid={guid} age={age} pdb={name}"


def strings(d, lo, hi):
    return set(m.group().decode() for m in re.finditer(rb"[\x20-\x7e]{6,}", d[lo:hi]))


def main():
    a, b = (open(p, "rb").read() for p in sys.argv[1:3])
    print(f"sizes: {len(a)} {len(b)}")
    print("A:", codeview(a))
    print("B:", codeview(b))
    for (name, raw, size, va), (_, raw_b, size_b, _) in zip(sections(a), sections(b)):
        x, y = a[raw:raw + size], b[raw_b:raw_b + size_b]
        if x == y:
            print(f"{name:10} same ({size} bytes)")
            continue
        diffs = [i for i in range(min(len(x), len(y))) if x[i] != y[i]]
        print(f"{name:10} DIFFERS: {len(diffs)} bytes (sizes {size}/{size_b}); first at +{diffs[0]:#x}" if diffs else f"{name:10} sizes differ {size}/{size_b}")
        for off in diffs[:5]:
            print(f"   +{off:#x}: {x[max(0, off - 16):off + 16].hex()}")
            print(f"   {' ' * len(hex(off))}  {y[max(0, off - 16):off + 16].hex()}")
        only_a = strings(a, raw, raw + size) - strings(b, raw_b, raw_b + size_b)
        only_b = strings(b, raw_b, raw_b + size_b) - strings(a, raw, raw + size)
        for s in sorted(only_a)[:10]:
            print(f"   only in A: {s!r}")
        for s in sorted(only_b)[:10]:
            print(f"   only in B: {s!r}")


if __name__ == "__main__":
    main()
