#!/usr/bin/env python3
"""List a PE file's DLL imports, and check the release rules.

  tools/pe_imports.py TempleOS.exe [--check]

Prints the normal and the delay-loaded imports. With --check, exits 1 unless
the exe is fit to ship: the WHPX DLLs are delay-loaded (so the exe starts
and explains itself on a PC without the Windows Hypervisor Platform), and
every normal import is part of Windows (no C runtime or MinGW DLLs to
install).
"""

import struct
import sys

# DLLs every supported Windows has. Anything else must not be a normal import.
SYSTEM = {
    "kernel32.dll", "user32.dll", "gdi32.dll", "ole32.dll", "oleaut32.dll",
    "ntdll.dll", "advapi32.dll", "bcryptprimitives.dll", "ws2_32.dll",
    "shell32.dll", "userenv.dll", "bcrypt.dll", "msvcrt.dll",
}
DELAYED = {"winhvplatform.dll", "winhvemulation.dll"}


def imports(data):
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError("not a PE file")
    nsect = struct.unpack_from("<H", data, pe + 6)[0]
    opt_size = struct.unpack_from("<H", data, pe + 20)[0]
    opt = pe + 24
    magic = struct.unpack_from("<H", data, opt)[0]
    dirs = opt + (112 if magic == 0x20B else 96)
    sections = []
    for i in range(nsect):
        s = opt + opt_size + 40 * i
        vsize, va, rsize, raw = struct.unpack_from("<IIII", data, s + 8)
        sections.append((va, max(vsize, rsize), raw))

    def off(rva):
        for va, size, raw in sections:
            if va <= rva < va + size:
                return rva - va + raw
        raise ValueError(f"rva {rva:#x} outside the sections")

    def name(rva):
        o = off(rva)
        return data[o:data.index(b"\0", o)].decode().lower()

    def table(index, entry_size, name_field):
        rva, size = struct.unpack_from("<II", data, dirs + 8 * index)
        out = []
        if not rva:
            return out
        o = off(rva)
        for _ in range(max(size // entry_size, 1)):
            entry = data[o:o + entry_size]
            if entry == bytes(entry_size):
                return out
            out.append(name(struct.unpack_from("<I", entry, name_field)[0]))
            o += entry_size
        return out

    # Directory 1: import table (20-byte entries, name RVA at +12).
    # Directory 13: delay import table (32-byte entries, name RVA at +4).
    return table(1, 20, 12), table(13, 32, 4)


def main():
    args = [a for a in sys.argv[1:] if a != "--check"]
    if len(args) != 1:
        sys.exit(__doc__)
    normal, delayed = imports(open(args[0], "rb").read())
    print("imports:      " + ", ".join(sorted(set(normal))))
    print("delay-loaded: " + (", ".join(sorted(set(delayed))) or "(none)"))
    if "--check" not in sys.argv:
        return 0
    bad = []
    for dll in sorted(set(normal)):
        if dll in DELAYED:
            bad.append(f"{dll} is imported at load time; it must be delay-loaded")
        elif dll not in SYSTEM and not dll.startswith("api-ms-win-"):
            bad.append(f"{dll} is not part of Windows")
    for dll in sorted(DELAYED - set(delayed)):
        if dll not in normal:
            bad.append(f"{dll} is not imported at all")
    for b in bad:
        print("FAIL: " + b)
    if not bad:
        print("OK")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
