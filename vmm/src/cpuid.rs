//! The CPU the guest sees: QEMU's `qemu64` model, leaf for leaf, as captured
//! from the reference machine by tools/qemu-ref/cpuid_ref.sh
//! (docs/ref/cpuid-qemu64.txt). Presenting the same leaves as the reference
//! keeps SeaBIOS and TempleOS on the same code paths, and hides host
//! details (brand string, cache topology, AVX...) that would otherwise leak
//! into the guest and differ between machines.

const TABLE: &str = include_str!("../../docs/ref/cpuid-qemu64.txt");

/// Leaves whose result depends on ECX.
const INDEXED: [u32; 5] = [0x4, 0x7, 0xB, 0xD, 0x8000_001D];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Regs {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

pub struct Cpuid {
    entries: Vec<(u32, u32, Regs)>,
}

impl Cpuid {
    pub fn qemu64() -> Self {
        Self::parse(TABLE)
    }

    fn parse(text: &str) -> Self {
        let entries = text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| {
                let f: Vec<u32> = l
                    .split(|c: char| c == ' ' || c == ':')
                    .filter(|s| !s.is_empty())
                    .map(|s| u32::from_str_radix(s, 16).expect("bad CPUID table entry"))
                    .collect();
                let [leaf, sub, eax, ebx, ecx, edx] = f[..] else {
                    panic!("bad CPUID table line: {l}")
                };
                (leaf, sub, Regs { eax, ebx, ecx, edx })
            })
            .collect();
        Cpuid { entries }
    }

    /// `qemu64` as QEMU 8.2 presents it with `-smp n` (one socket of n
    /// cores, one thread each): the logical processor count and topology
    /// bits change, nothing else (checked against captures for 2, 4 and 8
    /// CPUs in docs/ref/cpuid-qemu64-smpN.txt).
    pub fn qemu64_smp(n: u32) -> Self {
        let mut c = Self::qemu64();
        if n <= 1 {
            return c;
        }
        // Bits needed for a core ID: ceil(log2(n)).
        let bits = 32 - (n - 1).leading_zeros();
        for (leaf, sub, r) in &mut c.entries {
            match (*leaf, *sub) {
                (1, _) => {
                    r.ebx = (r.ebx & !0x00ff_0000) | (n.min(255) << 16);
                    r.edx |= 1 << 28; // HTT
                }
                (0xb, 1) => {
                    r.eax = bits;
                    r.ebx = n;
                }
                (0x8000_0001, _) => r.ecx |= 1 << 1, // CmpLegacy
                (0x8000_0008, _) => r.ecx = (bits << 12) | (n - 1),
                _ => {}
            }
        }
        c
    }

    /// Every leaf in the table; the hypervisor exits to us for these.
    pub fn leaves(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.entries.iter().map(|e| e.0).collect();
        v.dedup();
        v
    }

    /// Result of CPUID(leaf, subleaf) on the vCPU with local APIC ID `apic_id`.
    pub fn query(&self, leaf: u32, subleaf: u32, apic_id: u8) -> Regs {
        let sub = if INDEXED.contains(&leaf) { subleaf } else { 0 };
        let mut r = self
            .entries
            .iter()
            .find(|e| e.0 == leaf && e.1 == sub)
            .map(|e| e.2)
            .unwrap_or_else(|| match leaf {
                // Extended topology levels past the captured ones: invalid level, ECX echoes the index.
                0xB => Regs { eax: 0, ebx: 0, ecx: subleaf & 0xFF, edx: 0 },
                _ => Regs { eax: 0, ebx: 0, ecx: 0, edx: 0 },
            });
        match leaf {
            1 => r.ebx = (r.ebx & 0x00FF_FFFF) | (apic_id as u32) << 24,
            0xB => r.edx = apic_id as u32,
            _ => {}
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_and_long_mode() {
        let c = Cpuid::qemu64();
        let r = c.query(0, 0, 0);
        let mut vendor = Vec::new();
        for w in [r.ebx, r.edx, r.ecx] {
            vendor.extend_from_slice(&w.to_le_bytes());
        }
        assert_eq!(&vendor, b"AuthenticAMD");
        assert_ne!(c.query(0x8000_0001, 0, 0).edx & 1 << 29, 0, "long mode");
        assert_ne!(c.query(1, 0, 0).edx & 1 << 9, 0, "APIC");
        assert_eq!(c.query(1, 0, 3).ebx >> 24, 3);
    }

    fn table(text: &str) -> Vec<(u32, u32, Regs)> {
        Cpuid::parse(text).entries
    }

    #[test]
    fn smp_matches_qemu_captures() {
        for (n, text) in [
            (2, include_str!("../../docs/ref/cpuid-qemu64-smp2.txt")),
            (4, include_str!("../../docs/ref/cpuid-qemu64-smp4.txt")),
            (8, include_str!("../../docs/ref/cpuid-qemu64-smp8.txt")),
        ] {
            assert_eq!(Cpuid::qemu64_smp(n).entries, table(text), "-smp {n}");
        }
        assert_eq!(Cpuid::qemu64_smp(1).entries, Cpuid::qemu64().entries);
    }

    #[test]
    fn leaf_list_covers_basic_and_extended() {
        let l = Cpuid::qemu64().leaves();
        assert!(l.contains(&0) && l.contains(&0xD) && l.contains(&0x8000_000A));
        assert_eq!(l.iter().filter(|&&x| x == 7).count(), 1);
    }
}
