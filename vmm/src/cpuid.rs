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
        let entries = TABLE
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

    #[test]
    fn leaf_list_covers_basic_and_extended() {
        let l = Cpuid::qemu64().leaves();
        assert!(l.contains(&0) && l.contains(&0xD) && l.contains(&0x8000_000A));
        assert_eq!(l.iter().filter(|&&x| x == 7).count(), 1);
    }
}
