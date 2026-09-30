#!/usr/bin/env python3
"""Record the hard disk reference run the IDE disk model is checked against.

Builds ref/disk/initial.img (32 MiB: ata_probe.asm in the first sectors,
a byte pattern in sectors 100-109), copies it to ref/disk/disk.img and boots
QEMU from that copy with tools/qemu-ref/qemu_trace.py. QEMU's writes land in
disk.img, so afterwards:

  initial.img  the disk as the run started
  disk.img     the disk as QEMU left it
  trace.log    every access, including all IDE register traffic

Check the model against it (replays every IDE access, compares every read,
then compares the model's disk with disk.img byte for byte):

  TEMPLEOS_TRACE=ref/disk/trace.log TEMPLEOS_CD=none \\
  TEMPLEOS_HDD=ref/disk/initial.img TEMPLEOS_HDD_FINAL=ref/disk/disk.img \\
      cargo test -p devices replay_reference_trace -- --ignored

Needs nasm and qemu-system-x86_64.
"""

import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))


def main():
    out = os.path.join(ROOT, "ref", "disk")
    os.makedirs(out, exist_ok=True)
    probe = os.path.join(out, "ata_probe.bin")
    subprocess.run(["nasm", "-f", "bin", os.path.join(HERE, "ata_probe.asm"), "-o", probe], check=True)

    disk = bytearray(32 << 20)
    code = open(probe, "rb").read()
    disk[: len(code)] = code
    for i in range(10 * 512):
        disk[100 * 512 + i] = (i * 13 + 5) & 0xFF
    initial = os.path.join(out, "initial.img")
    with open(initial, "wb") as f:
        f.write(disk)
    shutil.copyfile(initial, os.path.join(out, "disk.img"))

    subprocess.run([sys.executable, os.path.join(HERE, "qemu_trace.py"), "--iso", "none",
                    "--hdd", os.path.join(out, "disk.img"), "--seconds", "5",
                    "--shot-every", "5", "--out", out], check=True)
    log = open(os.path.join(out, "debugcon.log"), "rb").read()
    if not log.endswith(b"SD"):
        print(f"warning: probe did not finish (debug console ends {log[-40:]!r})", file=sys.stderr)
        return 1
    print("probe finished")
    return 0


if __name__ == "__main__":
    sys.exit(main())
