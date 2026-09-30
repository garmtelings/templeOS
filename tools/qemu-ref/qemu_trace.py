#!/usr/bin/env python3
"""Boot TempleOS in QEMU and record a reference trace.

The trace is the ground truth our own VMM is compared against (see PLAN.md,
Phase 6). The machine configuration here is the one the VMM reproduces:

  * i440FX + PIIX3 ("-machine pc"), HPET on, SMM off (WHPX has no SMM,
    and QEMU turns it off too when it runs on WHPX), 1 CPU by default
  * the pinned SeaBIOS / SeaVGABIOS from payload/
  * the ISO on the secondary IDE master (same slot as QEMU's -cdrom)
  * instruction counting (-icount) and a fixed RTC base, so runs are
    repeatable

Outputs, in --out:
  trace.log      raw QEMU trace (port I/O, MMIO, PCI config, IRQs)
  debugcon.log   SeaBIOS debug output (port 0x402)
  shot-NNNN.ppm  screen dumps every --shot-every seconds (guest virtual time)
  manifest.json  command line, QEMU version, input hashes, screenshot hashes
"""

import argparse
import calendar
import hashlib
import json
import math
import os
import socket
import subprocess
import sys
import time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
PAYLOAD = os.path.join(ROOT, "payload")
HERE = os.path.dirname(os.path.abspath(__file__))


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


class QMP:
    """QMP client. `addr` is a Unix socket path, or a (host, port) pair
    (Windows, where Python has no Unix sockets)."""

    def __init__(self, addr, timeout):
        deadline = time.monotonic() + timeout
        family = socket.AF_INET if isinstance(addr, tuple) else socket.AF_UNIX
        while True:
            try:
                self.sock = socket.socket(family, socket.SOCK_STREAM)
                self.sock.connect(addr)
                break
            except OSError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.05)
        self.buf = b""
        self._recv()  # greeting
        self.cmd("qmp_capabilities")

    def _recv(self):
        while b"\n" not in self.buf:
            data = self.sock.recv(65536)
            if not data:
                raise EOFError("QMP connection closed")
            self.buf += data
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line)

    def cmd(self, name, **args):
        msg = {"execute": name}
        if args:
            msg["arguments"] = args
        self.sock.sendall(json.dumps(msg).encode() + b"\n")
        while True:
            reply = self._recv()
            if "return" in reply or "error" in reply:
                if "error" in reply:
                    raise RuntimeError(f"QMP {name}: {reply['error']}")
                return reply["return"]


def guest_seconds(qmp):
    """The guest's RTC in whole seconds. With -rtc clock=vm it runs on
    QEMU's virtual clock, which under -icount counts instructions."""
    t = qmp.cmd("qom-get", path="/machine", property="rtc-time")
    return calendar.timegm((t["tm_year"] + 1900, t["tm_mon"] + 1, t["tm_mday"],
                            t["tm_hour"], t["tm_min"], t["tm_sec"]))


def guest_wait(qmp, seconds):
    """Wait until at least `seconds` of guest time have passed (the RTC
    ticks in whole seconds, so it can be up to one second more)."""
    until = guest_seconds(qmp) + math.ceil(seconds) + 1
    while guest_seconds(qmp) < until:
        time.sleep(0.05)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--iso", default=os.path.join(PAYLOAD, "TempleOS.ISO"),
                    help="ISO to boot (default: payload/TempleOS.ISO); "
                         "pass 'none' to trace the BIOS alone")
    ap.add_argument("--hdd", help="raw hard disk image on primary IDE master")
    ap.add_argument("--mem", default="1024", help="guest RAM in MiB")
    ap.add_argument("--smp", type=int, default=1)
    ap.add_argument("--seconds", type=float, default=60.0,
                    help="host seconds to run before quitting")
    ap.add_argument("--shot-every", type=float, default=5.0)
    ap.add_argument("--no-icount", action="store_true",
                    help="run free (faster, not repeatable)")
    ap.add_argument("--out", required=True)
    ap.add_argument("--script", help="run this input script (see "
                    "devices/src/script.rs) instead of periodic screenshots")
    ap.add_argument("--wait-scale", type=float, default=1.0,
                    help="with --no-icount, multiply script waits (QEMU's "
                         "software CPU is slower); with -icount, script waits "
                         "are guest time and this is ignored")
    ap.add_argument("--icount-sleep", action="store_true",
                    help="-icount with sleep=on: while the guest idles its clock "
                         "follows the host's, as on WHPX, so the host-timed gaps "
                         "between script keys are the same guest time on both "
                         "machines (sleep=off jumps the clock ahead while idle; "
                         "repeatable, but a 20 ms key gap becomes any length)")
    ap.add_argument("--no-trace", action="store_true",
                    help="don't record trace.log (script runs only need the "
                         "screenshots, and tracing makes QEMU much slower)")
    ap.add_argument("--qemu", default="qemu-system-x86_64")
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    if hasattr(socket, "AF_UNIX"):
        qmp_addr = os.path.join(args.out, "qmp.sock")
        if os.path.exists(qmp_addr):
            os.unlink(qmp_addr)
        qmp_arg = f"unix:{qmp_addr},server=on,wait=off"
    else:
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            qmp_addr = ("127.0.0.1", s.getsockname()[1])
        qmp_arg = f"tcp:127.0.0.1:{qmp_addr[1]},server=on,wait=off"

    bios = os.path.join(PAYLOAD, "bios.bin")
    vgabios = os.path.join(PAYLOAD, "vgabios.bin")
    cmd = [
        args.qemu,
        "-machine", "pc,hpet=on,smm=off",
        "-cpu", "qemu64",
        "-m", args.mem,
        "-smp", str(args.smp),
        "-bios", bios,
        "-vga", "none",
        "-device", f"VGA,romfile={vgabios}",
        "-rtc", "base=2017-11-20T00:00:00,clock=vm",
        "-display", "none",
        "-no-reboot",
        "-nodefaults",
        "-qmp", qmp_arg,
        "-chardev", f"file,id=dbg,path={os.path.join(args.out, 'debugcon.log')}",
        "-device", "isa-debugcon,iobase=0x402,chardev=dbg",
        "-S",  # start paused so the first screenshot is at t=0
    ]
    trace = os.path.join(args.out, "trace.log")
    if os.path.exists(trace):
        os.unlink(trace)
    if not args.no_trace:
        cmd += ["-trace", f"events={os.path.join(HERE, 'events.txt')},file={trace}"]
    if not args.no_icount:
        sleep = "on" if args.icount_sleep else "off"
        cmd += ["-icount", f"shift=0,sleep={sleep},align=off"]
    iso = None if args.iso == "none" else args.iso
    if iso:
        cmd += ["-drive", f"file={iso},media=cdrom,if=ide,index=2,readonly=on"]
        # CD only: boot it. With a hard disk too, keep QEMU's default order
        # (disk first), which is what templeos.exe does.
        if not args.hdd:
            cmd += ["-boot", "d"]
    if args.hdd:
        cmd += ["-drive", f"file={args.hdd},format=raw,if=ide,index=0"]

    inputs = {"bios.bin": sha256(bios), "vgabios.bin": sha256(vgabios)}
    if iso:
        inputs[os.path.basename(iso)] = sha256(iso)
    version = subprocess.run([args.qemu, "--version"], capture_output=True,
                             text=True).stdout.splitlines()[0]

    proc = subprocess.Popen(cmd)
    shots = []
    try:
        qmp = QMP(qmp_addr, timeout=10)
        qmp.cmd("cont")
        start = time.monotonic()
        n = 0
        if args.script:
            sys.path.insert(0, HERE)
            import inputscript
            steps = inputscript.parse(open(args.script).read())

            def on_shot(path):
                shots.append({"file": os.path.basename(path),
                              "host_seconds": round(time.monotonic() - start, 3),
                              "sha256": sha256(path) if os.path.exists(path) else None})
            wait = None if args.no_icount else (lambda s: guest_wait(qmp, s))
            inputscript.run(steps, qmp, args.out, args.wait_scale, on_shot, wait)
        while not args.script:
            elapsed = time.monotonic() - start
            path = os.path.join(args.out, f"shot-{n:04d}.ppm")
            qmp.cmd("screendump", filename=path)
            shots.append({"file": os.path.basename(path),
                          "host_seconds": round(elapsed, 3),
                          "sha256": sha256(path) if os.path.exists(path) else None})
            n += 1
            if elapsed >= args.seconds or proc.poll() is not None:
                break
            time.sleep(min(args.shot_every, max(0.0, args.seconds - elapsed)))
        try:
            qmp.cmd("quit")
        except (EOFError, OSError):
            pass
    finally:
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()

    manifest = {"qemu": version, "command": cmd, "inputs": inputs,
                "screenshots": shots}
    with open(os.path.join(args.out, "manifest.json"), "w") as f:
        json.dump(manifest, f, indent=2)
    size = f"trace {os.path.getsize(trace)} bytes" if os.path.exists(trace) else "no trace"
    print(f"wrote {args.out}: {len(shots)} screenshots, {size}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
