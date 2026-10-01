#!/usr/bin/env bash
# Run every tests/scripts/*.script on the software CPU (vmm example softrun)
# and on QEMU (tools/qemu-ref/qemu_trace.py, -icount), and compare the
# screenshots pixel for pixel, with the same masks as
# tools/phase6-windows.ps1. Both machines wait in guest time = instruction
# count, so the runs repeat exactly.
#
# Needs payload/TempleOS.ISO, qemu-system-x86_64 (8.2) and python3.
# Results in soft-results/; exits 1 if any screenshot differs.
set -uo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
out=soft-results
rm -rf "$out"
mkdir -p "$out"
cargo build -q --release -p vmm --example softrun || exit 1
fail=0
for script in tests/scripts/*.script; do
  name=$(basename "$script" .script)
  echo "== $name"
  start=$(date +%s)
  if ! target/release/examples/softrun --script "$script" --shots "$out/$name/ours" > "$out/$name-soft.log" 2>&1; then
    echo "FAIL $name: software CPU run (see $out/$name-soft.log)"; tail -5 "$out/$name-soft.log"; fail=1; continue
  fi
  echo "   software CPU: $(( $(date +%s) - start ))s; $(tail -1 "$out/$name-soft.log")"
  start=$(date +%s)
  if ! python3 tools/qemu-ref/qemu_trace.py --script "$script" --no-trace --out "$out/$name/qemu" > "$out/$name-qemu.log" 2>&1; then
    echo "FAIL $name: QEMU run (see $out/$name-qemu.log)"; tail -5 "$out/$name-qemu.log"; fail=1; continue
  fi
  echo "   QEMU: $(( $(date +%s) - start ))s"
  masks=()
  [ -f "tests/scripts/$name.masks" ] && masks=(--masks "tests/scripts/$name.masks")
  if python3 tools/compare_shots.py "$out/$name/ours" "$out/$name/qemu" --mask 0:8 "${masks[@]}"; then
    echo "PASS $name"
  else
    echo "FAIL $name"; fail=1
  fi
done
exit $fail
