use super::CRC32;

/// The SNES internal checksum of `rom`: the 16-bit sum of every byte, with a
/// non-power-of-two ROM's tail mirrored up to the next power of two (e.g.
/// 3 MB = 2 MB + the last 1 MB counted twice).
pub fn rom_checksum(rom: &[u8]) -> u16 {
    let mut size = 1;
    while size < rom.len() {
        size <<= 1;
    }
    mirrored_sum(rom, size) as u16
}

fn mirrored_sum(rom: &[u8], size: usize) -> u64 {
    if rom.is_empty() {
        return 0;
    }
    let mut p = 1;
    while p < rom.len() {
        p <<= 1;
    }
    let sum = if p == rom.len() {
        rom.iter().map(|&b| b as u64).sum()
    } else {
        let half = p / 2;
        let a: u64 = rom[..half].iter().map(|&b| b as u64).sum();
        a + mirrored_sum(&rom[half..], half)
    };
    sum * (size / p) as u64
}

pub fn verify_checksum(rom: &[u8], want: u16) -> (bool, u16) {
    if rom.is_empty() {
        return (false, 0);
    }
    let got = rom_checksum(rom);
    (got == want, got)
}

/// Candidate smaller sizes (bytes, largest first) to re-verify a dump whose
/// checksum failed: carts sometimes report a larger size than they actually
/// are.
pub fn trimmed_sizes(size: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for mb in [8.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.5] {
        let s = (mb * 1024.0 * 1024.0) as usize;
        if s < size && s >= 0x2000 && seen.insert(s) {
            out.push(s);
        }
    }
    out.sort_unstable_by(|a, b| b.cmp(a));
    out
}

/// The CRC32 of a specific known-bad large cart. The Windows client
/// special-cases it (0x4585c0): if the dumped ROM's CRC32 equals this value,
/// byte 0xB06BFB is patched to 6.
const KNOWN_BAD_CART_CRC: u32 = 0xBD79_3072;

pub fn apply_known_bad_cart_patch(rom: &mut [u8]) -> bool {
    if rom.len() <= 0xB06BFB {
        return false;
    }
    if CRC32.checksum(rom) != KNOWN_BAD_CART_CRC {
        return false;
    }
    rom[0xB06BFB] = 6;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_power_of_two() {
        // A 256-byte ROM of all 0x01: sum = 256.
        let rom = vec![0x01u8; 256];
        assert_eq!(rom_checksum(&rom), 0x0100);
    }

    #[test]
    fn checksum_mirrors_non_power_of_two() {
        // A 3-byte ROM mirrored to 4: the byte at offset 2 (the "tail") is
        // counted twice.
        let rom = [0x10u8, 0x20, 0x40];
        // Mirrored to 4: [0x10, 0x20, 0x40, 0x40] -> sum = 0x10+0x20+0x40+0x40 = 0xB0.
        assert_eq!(rom_checksum(&rom), 0x00B0);
    }

    #[test]
    fn verify_checksum_match_and_mismatch() {
        let rom = vec![0x01u8; 256];
        assert_eq!(verify_checksum(&rom, 0x0100), (true, 0x0100));
        assert_eq!(verify_checksum(&rom, 0x0000), (false, 0x0100));
        assert_eq!(verify_checksum(&[], 0), (false, 0));
    }

    #[test]
    fn trimmed_sizes_are_largest_first_and_smaller() {
        let sizes = trimmed_sizes(8 * 1024 * 1024);
        assert!(sizes.iter().all(|&s| s < 8 * 1024 * 1024));
        for w in sizes.windows(2) {
            assert!(w[0] > w[1]);
        }
    }
}
