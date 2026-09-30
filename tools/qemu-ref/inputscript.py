"""Input scripts for the QEMU reference machine: the Python side of
devices/src/script.rs (same grammar, same US-layout typing, same pacing).

Keep the two in step: tools/qemu-ref/input_ref.py records a run whose
keyboard bytes the Rust side must reproduce exactly.
"""

import time

PLAIN = "`1234567890-=qwertyuiop[]\\asdfghjkl;'zxcvbnm,./"
SHIFTED = "~!@#$%^&*()_+QWERTYUIOP{}|ASDFGHJKL:\"ZXCVBNM<>?"
KEYS = ["grave_accent", "1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "minus", "equal",
        "q", "w", "e", "r", "t", "y", "u", "i", "o", "p", "bracket_left", "bracket_right",
        "backslash", "a", "s", "d", "f", "g", "h", "j", "k", "l", "semicolon", "apostrophe",
        "z", "x", "c", "v", "b", "n", "m", "comma", "dot", "slash"]

# Host seconds between key events (both runners use the same pacing, so
# neither machine's 16-byte keyboard queue overflows).
KEY_GAP = 0.02


def strip_comment(s):
    for i, c in enumerate(s):
        if c == "#" and (i == 0 or s[i - 1] != "\\"):
            return s[:i]
    return s


def unescape(s):
    out, i = [], 0
    while i < len(s):
        c = s[i]
        if c == "\\" and i + 1 < len(s):
            i += 1
            out.append({"n": "\n", "t": "\t"}.get(s[i], s[i]))
        else:
            out.append(c)
        i += 1
    return "".join(out)


def char_key(c):
    if c == " ":
        return "spc", False
    if c == "\n":
        return "ret", False
    if c == "\t":
        return "tab", False
    if c in PLAIN:
        return KEYS[PLAIN.index(c)], False
    if c in SHIFTED:
        return KEYS[SHIFTED.index(c)], True
    raise ValueError(f"can't type {c!r}")


def type_text(text):
    ev = []
    for c in text:
        key, shift = char_key(c)
        if shift:
            ev.append(("shift", True))
        ev += [(key, True), (key, False)]
        if shift:
            ev.append(("shift", False))
    return ev


def parse(text):
    steps = []
    for n, raw in enumerate(text.splitlines(), 1):
        line = strip_comment(raw).strip()
        if not line:
            continue
        cmd, _, rest = line.partition(" ")
        rest = rest.strip()
        if cmd == "wait":
            steps.append(("wait", float(rest)))
        elif cmd == "type":
            arg = raw.lstrip()[len("type"):]
            arg = arg[1:] if arg.startswith(" ") else arg
            steps.append(("keys", type_text(unescape(strip_comment(arg).rstrip("\r")))))
        elif cmd == "key":
            names = [k.strip() for k in rest.split("+")]
            steps.append(("keys", [(k, True) for k in names] + [(k, False) for k in reversed(names)]))
        elif cmd == "mouse":
            f = rest.split()
            steps.append(("mouse", int(f[0]), int(f[1]), int(f[2]) if len(f) > 2 else 0))
        elif cmd == "screenshot":
            steps.append(("screenshot", rest))
        else:
            raise ValueError(f"line {n}: unknown command: {raw}")
    return steps


BUTTONS = [(1, "left"), (2, "right"), (4, "middle")]


def run(steps, qmp, out_dir, wait_scale=1.0, on_shot=None):
    """Execute steps against a QEMU QMP connection."""
    held = 0
    for step in steps:
        kind = step[0]
        if kind == "wait":
            time.sleep(step[1] * wait_scale)
        elif kind == "keys":
            for key, down in step[1]:
                qmp.cmd("input-send-event", events=[
                    {"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": key}}}])
                time.sleep(KEY_GAP)
        elif kind == "mouse":
            _, dx, dy, buttons = step
            events = [{"type": "rel", "data": {"axis": "x", "value": dx}},
                      {"type": "rel", "data": {"axis": "y", "value": dy}}]
            for bit, name in BUTTONS:
                if (buttons ^ held) & bit:
                    events.append({"type": "btn", "data": {"down": bool(buttons & bit), "button": name}})
            held = buttons
            qmp.cmd("input-send-event", events=events)
            time.sleep(KEY_GAP)
        elif kind == "screenshot":
            path = f"{out_dir}/{step[1]}.ppm"
            qmp.cmd("screendump", filename=path)
            if on_shot:
                on_shot(path)
