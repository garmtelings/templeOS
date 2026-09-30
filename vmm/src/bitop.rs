//! BT, BTS, BTR and BTC with a memory operand on device memory.
//!
//! WHPX's instruction emulator doesn't implement the bit-test instructions,
//! but TempleOS uses them on VGA memory: `LBts(text.vga_alias, ...)` in the
//! `ScrnMemory` demo is `LOCK BTS [RCX], R8`. When the emulator gives up on
//! an MMIO access, the VMM decodes the instruction here, and the bit
//! operation itself runs on the host CPU, so the result and every flag
//! (including the ones Intel leaves undefined) are what this CPU gives the
//! guest for the same instruction on RAM.
//!
//! The operand's address isn't decoded: it is the GPA of the access that
//! exited, which the CPU has already computed (including a register bit
//! offset's move to another word).

/// Which bit-test instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Test,
    Set,
    Reset,
    Complement,
}

/// Where the bit number comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitSource {
    /// A general-purpose register, 0-15 in x86 encoding order (RAX, RCX,
    /// RDX, RBX, RSP, RBP, RSI, RDI, R8-R15).
    Reg(u8),
    Imm(u8),
}

/// A decoded bit-test instruction with a memory operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitInsn {
    pub op: Op,
    /// Operand size in bytes: 2, 4 or 8.
    pub size: u8,
    pub bit: BitSource,
    /// Instruction length in bytes.
    pub len: u8,
}

impl BitInsn {
    /// The bit within the operand for bit number `n` (the register's value
    /// or the immediate): the CPU has already applied the part of a
    /// register offset that selects the operand's address.
    pub fn bit_index(&self, n: u64) -> u64 {
        n & (self.size as u64 * 8 - 1)
    }
}

/// Decode `bytes` (fetched at RIP) as BT/BTS/BTR/BTC with a memory operand,
/// for code of `mode` bits (16, 32 or 64). None for anything else.
pub fn decode(bytes: &[u8], mode: u8) -> Option<BitInsn> {
    let mut i = 0;
    let (mut opsize_override, mut addrsize_override) = (false, false);
    // Legacy prefixes, in any order.
    while let Some(&b) = bytes.get(i) {
        match b {
            0x66 => opsize_override = true,
            0x67 => addrsize_override = true,
            0xF0 | 0xF2 | 0xF3 | 0x26 | 0x2E | 0x36 | 0x3E | 0x64 | 0x65 => {}
            _ => break,
        }
        i += 1;
    }
    // REX counts only right before the opcode.
    let rex = match bytes.get(i) {
        Some(&b @ 0x40..=0x4F) if mode == 64 => {
            i += 1;
            b
        }
        _ => 0,
    };
    if bytes.get(i) != Some(&0x0F) {
        return None;
    }
    let opcode = *bytes.get(i + 1)?;
    let modrm = *bytes.get(i + 2)?;
    i += 3;
    let (md, reg, rm) = (modrm >> 6, (modrm >> 3) & 7, modrm & 7);
    if md == 3 {
        return None; // register operand: never an MMIO exit
    }
    let (op, bit) = match opcode {
        0xA3 => (Op::Test, None),
        0xAB => (Op::Set, None),
        0xB3 => (Op::Reset, None),
        0xBB => (Op::Complement, None),
        0xBA => {
            let op = match reg {
                4 => Op::Test,
                5 => Op::Set,
                6 => Op::Reset,
                7 => Op::Complement,
                _ => return None,
            };
            (op, Some(()))
        }
        _ => return None,
    };
    let size = match mode {
        64 if rex & 8 != 0 => 8,
        16 => {
            if opsize_override {
                4
            } else {
                2
            }
        }
        _ => {
            if opsize_override {
                2
            } else {
                4
            }
        }
    };
    let addr16 = match mode {
        64 => false,
        32 => addrsize_override,
        _ => !addrsize_override,
    };
    // Addressing bytes after ModRM.
    if addr16 {
        i += match md {
            0 if rm == 6 => 2,
            1 => 1,
            2 => 2,
            _ => 0,
        };
    } else {
        let mut base = rm;
        if rm == 4 {
            base = *bytes.get(i)? & 7;
            i += 1;
        }
        i += match md {
            0 if base == 5 => 4, // disp32 (RIP-relative when rm == 5 in 64-bit mode)
            1 => 1,
            2 => 4,
            _ => 0,
        };
    }
    let bit = match bit {
        Some(()) => {
            let imm = *bytes.get(i)?;
            i += 1;
            BitSource::Imm(imm)
        }
        None => BitSource::Reg(reg | (rex & 4) << 1),
    };
    if i > 15 {
        return None;
    }
    Some(BitInsn { op, size, bit, len: i as u8 })
}

/// The arithmetic flags (CF, PF, AF, ZF, SF, OF).
const ARITH_FLAGS: u64 = 0x8D5;

/// Run `op` on `value` (an operand of `size` bytes) and bit `bit`
/// (already reduced by [`BitInsn::bit_index`]) on the host CPU, starting
/// from the guest's arithmetic flags. Returns the new operand value and the
/// guest's new RFLAGS.
#[cfg(target_arch = "x86_64")]
pub fn execute(op: Op, size: u8, value: u64, bit: u64, rflags: u64) -> (u64, u64) {
    use core::arch::asm;
    let mut v = value;
    // Only the arithmetic flags go through POPFQ: nothing else in the host's
    // RFLAGS changes.
    let mut f = (rflags & ARITH_FLAGS) | 2;
    macro_rules! run {
        ($insn:literal, $modifier:literal) => {
            // SAFETY: a register-only bit op between a flags push and pop;
            // the stack is balanced and POPFQ sets only arithmetic flags.
            unsafe {
                asm!(
                    "push {f}",
                    "popfq",
                    concat!($insn, " {v:", $modifier, "}, {b:", $modifier, "}"),
                    "pushfq",
                    "pop {f}",
                    v = inout(reg) v,
                    b = in(reg) bit,
                    f = inout(reg) f,
                )
            }
        };
    }
    macro_rules! sized {
        ($insn:literal) => {
            match size {
                2 => run!($insn, "x"),
                4 => run!($insn, "e"),
                _ => run!($insn, "r"),
            }
        };
    }
    match op {
        Op::Test => sized!("bt"),
        Op::Set => sized!("bts"),
        Op::Reset => sized!("btr"),
        Op::Complement => sized!("btc"),
    }
    // A 32-bit op zeroes the register's upper half; the operand is only
    // `size` bytes anyway.
    let mask = if size == 8 { u64::MAX } else { (1u64 << (size * 8)) - 1 };
    (v & mask, (rflags & !ARITH_FLAGS) | (f & ARITH_FLAGS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_templeos_lbts() {
        // LOCK BTS [RCX], R8 from ScrnMemory.
        let insn = decode(&[0xF0, 0x4C, 0x0F, 0xAB, 0x01, 0x5F, 0x5E], 64).unwrap();
        assert_eq!(insn, BitInsn { op: Op::Set, size: 8, bit: BitSource::Reg(8), len: 5 });
    }

    #[test]
    fn decodes_forms_and_lengths() {
        // BT [RAX], ECX
        assert_eq!(decode(&[0x0F, 0xA3, 0x08], 64), Some(BitInsn { op: Op::Test, size: 4, bit: BitSource::Reg(1), len: 3 }));
        // BTR WORD [RBX+0x10], DX
        assert_eq!(decode(&[0x66, 0x0F, 0xB3, 0x53, 0x10], 64), Some(BitInsn { op: Op::Reset, size: 2, bit: BitSource::Reg(2), len: 5 }));
        // BTC QWORD [RSP+RSI*4+0x12345678], R15
        assert_eq!(
            decode(&[0x4C, 0x0F, 0xBB, 0xBC, 0xB4, 0x78, 0x56, 0x34, 0x12], 64),
            Some(BitInsn { op: Op::Complement, size: 8, bit: BitSource::Reg(15), len: 9 })
        );
        // BTS DWORD [RIP+disp32], 5
        assert_eq!(
            decode(&[0x0F, 0xBA, 0x2D, 1, 2, 3, 4, 5], 64),
            Some(BitInsn { op: Op::Set, size: 4, bit: BitSource::Imm(5), len: 8 })
        );
        // SIB with no base: BT [disp32+RAX*8], 3
        assert_eq!(
            decode(&[0x0F, 0xBA, 0x24, 0xC5, 0, 0, 0xA, 0, 3], 64),
            Some(BitInsn { op: Op::Test, size: 4, bit: BitSource::Imm(3), len: 9 })
        );
        // 16-bit code: BTS [0x1234], AX
        assert_eq!(decode(&[0x0F, 0xAB, 0x06, 0x34, 0x12], 16), Some(BitInsn { op: Op::Set, size: 2, bit: BitSource::Reg(0), len: 5 }));
        // 32-bit code with a 16-bit address: BTR [BX+SI+0x12], ECX
        assert_eq!(decode(&[0x67, 0x0F, 0xB3, 0x48, 0x12], 32), Some(BitInsn { op: Op::Reset, size: 4, bit: BitSource::Reg(1), len: 5 }));
    }

    #[test]
    fn rejects_other_instructions() {
        assert_eq!(decode(&[0x0F, 0xAB, 0xC1], 64), None); // register operand
        assert_eq!(decode(&[0x0F, 0xBA, 0x18, 1], 64), None); // 0F BA /3: not a bit test
        assert_eq!(decode(&[0x89, 0x01], 64), None); // MOV
        assert_eq!(decode(&[0x0F, 0xAB], 64), None); // truncated
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn executes_like_the_cpu() {
        let (v, f) = execute(Op::Set, 8, 0x10, 7, 0x202);
        assert_eq!((v, f & 1), (0x90, 0));
        let (v, f) = execute(Op::Set, 8, 0x90, 7, 0x202);
        assert_eq!((v, f & 1), (0x90, 1));
        let (v, f) = execute(Op::Reset, 4, 0xFFFF_FFFF, 31, 0x2);
        assert_eq!((v, f & 1), (0x7FFF_FFFF, 1));
        let (v, f) = execute(Op::Complement, 2, 0x0001, 0, 0x2);
        assert_eq!((v, f & 1), (0x0000, 1));
        let (v, f) = execute(Op::Test, 8, 1 << 63, 63, 0x2);
        assert_eq!((v, f & 1), (1 << 63, 1));
        // Flags other than the arithmetic ones are the guest's, untouched.
        let (_, f) = execute(Op::Test, 4, 0, 0, 0x200 | 0x400 | 0x40);
        assert_eq!(f & !ARITH_FLAGS, 0x600);
    }
}
