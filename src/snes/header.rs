//! SNES cartridge header (the 0x2000-byte block at $E000).
//!
//! $00:FFC0 is the internal header for LoROM, HiROM and ExHiROM carts alike
//! (HiROM bank $C0 is mirrored into the upper half of bank $00).
//!
//! NOTE the checksum field offsets: the checksum is at header offset 0x1E
//! ($FFDE) and the complement at 0x1C ($FFDC) — not the 0x7C/0x7D that many
//! references cite.

use super::CRC32;

#[derive(Debug, Clone, Default)]
pub struct Header {
    /// $FFC0..$FFD4 (21 bytes).
    pub title: String,
    /// $FFD5 (0x20/0x30 LoROM, 0x21/0x31 HiROM, 0x25/0x35 ExHiROM, ...).
    pub map_mode: u8,
    /// $FFD6 cartridge type.
    pub chip: u8,
    /// $FFD7 ROM size, 1 KiB << code.
    pub size_code: u8,
    /// $FFD8 SRAM size, 1 KiB << code (0 = none).
    pub sram_code: u8,
    /// $FFD9.
    pub region: u8,
    /// $FFDA developer ID (0x33 = extended header present).
    pub license: u8,
    /// $FFDB mask ROM version.
    pub version: u8,
    /// $FFDC/$FFDD checksum complement.
    pub complement: u16,
    /// $FFDE/$FFDF checksum.
    pub checksum: u16,
    /// $FFB0..$FFB1 (extended header).
    pub maker_code: [u8; 2],
    /// $FFB2..$FFB5 (extended header).
    pub game_code: [u8; 4],
    /// $FFBD expansion RAM size, 1 KiB << code (extended header).
    pub exp_ram_code: u8,
    /// $FFBF chip subtype (extended header; 3 = CX4).
    pub subtype: u8,
    /// Homebrew Flashcard V1 simple signature.
    pub homebrew: bool,
    /// CRC32 of the whole $E000 block: the key of the official client's cart
    /// database (FUN_0045d270 -> FUN_00472560).
    pub block_crc: u32,
    /// Force the ROM size for Homebrew Flashcard V1 (0 = not overridden).
    pub(crate) rom_override: usize,
    /// Force the SRAM size for Homebrew Flashcard V1 (0 = not overridden).
    pub(crate) sram_override: usize,
}

impl Header {
    pub fn parse(block: &[u8]) -> Self {
        let mut buf = [0u8; 0x2000];
        buf[..block.len().min(0x2000)].copy_from_slice(&block[..block.len().min(0x2000)]);
        let block = &buf[..];
        let mut h = Header {
            block_crc: CRC32.checksum(block),
            ..Default::default()
        };
        h.title = title_string(&block[0x1FC0..0x1FC0 + 21]);
        h.map_mode = block[0x1FD5];
        h.chip = block[0x1FD6];
        h.size_code = block[0x1FD7];
        h.sram_code = block[0x1FD8];
        h.region = block[0x1FD9];
        h.license = block[0x1FDA];
        h.version = block[0x1FDB];
        h.complement = u16::from_le_bytes([block[0x1FDC], block[0x1FDD]]);
        h.checksum = u16::from_le_bytes([block[0x1FDE], block[0x1FDF]]);
        if h.license == 0x33 {
            h.maker_code.copy_from_slice(&block[0x1FB0..0x1FB2]);
            h.game_code.copy_from_slice(&block[0x1FB2..0x1FB6]);
            h.exp_ram_code = block[0x1FBD];
            h.subtype = block[0x1FBF];
        }
        // Homebrew Flashcard V1 simple: $FFC0 == "MENU" and $FFDC == 0xF8BB0744
        // (little-endian), per 0x4735f0.
        h.homebrew = &block[0x1FC0..0x1FC4] == b"MENU"
            && u32::from_le_bytes([block[0x1FDC], block[0x1FDD], block[0x1FDE], block[0x1FDF]])
                == 0xF8BB_0744;
        h
    }

    /// True when the header is self-consistent: checksum and complement add up
    /// to 0xFFFF, the map mode is in the $20-$3F range and the ROM size code
    /// is plausible (256 Kbit..128 Mbit).
    pub fn valid(&self) -> bool {
        self.checksum ^ self.complement == 0xFFFF
            && self.map_mode & 0xE0 == 0x20
            && self.size_code >= 0x05
            && self.size_code <= 0x0E
    }

    pub fn rom_size(&self) -> usize {
        if self.rom_override != 0 {
            return self.rom_override;
        }
        if self.size_code > 0x0E {
            return 0;
        }
        0x400 << self.size_code
    }

    pub fn sram_size(&self) -> usize {
        if self.sram_override != 0 {
            return self.sram_override;
        }
        if self.sram_code == 0 || self.sram_code > 0x0A {
            return 0;
        }
        0x400 << self.sram_code
    }

    pub fn exp_ram_size(&self) -> usize {
        if self.exp_ram_code == 0 || self.exp_ram_code > 0x0A {
            return 0;
        }
        0x400 << self.exp_ram_code
    }

    /// The $FFD6 coprocessor nibble, if the cartridge type has one (low
    /// nibble 0-2 has no coprocessor).
    fn coprocessor(&self) -> Option<u8> {
        if self.chip & 0x0F < 3 {
            None
        } else {
            Some(self.chip >> 4)
        }
    }

    pub fn is_sfx(&self) -> bool {
        self.coprocessor() == Some(0x1)
    }

    pub fn is_sa1(&self) -> bool {
        self.coprocessor() == Some(0x3)
    }
}

pub(crate) fn title_for(block: &[u8]) -> String {
    title_string(&block[0x1FC0..0x1FC0 + 21])
}

fn title_string(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let mut s = &b[..end];
    while s.last() == Some(&b' ') {
        s = &s[..s.len() - 1];
    }
    String::from_utf8_lossy_owned(s.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_trims_nul_and_spaces() {
        assert_eq!(
            title_string(b"SUPER MARIO WORLD      "),
            "SUPER MARIO WORLD"
        );
        assert_eq!(title_string(b"ABC\x00DEF"), "ABC");
        assert_eq!(title_string(b"  "), "");
    }

    #[test]
    fn parse_and_valid() {
        let mut block = [0u8; 0x2000];
        block[0x1FC0..0x1FC0 + 4].copy_from_slice(b"TEST");
        block[0x1FD5] = 0x20; // LoROM
        block[0x1FD7] = 0x09; // 512 KB
        block[0x1FDC] = 0x25; // complement low byte -> value 0x5F25
        block[0x1FDD] = 0x5F; // complement high byte
        block[0x1FDE] = 0xDA; // checksum low byte -> value 0xA0DA
        block[0x1FDF] = 0xA0; // checksum high byte
        let h = Header::parse(&block);
        assert_eq!(h.title, "TEST");
        assert_eq!(h.map_mode, 0x20);
        assert_eq!(h.checksum, 0xA0DA);
        assert_eq!(h.complement, 0x5F25);
        assert!(h.valid());
        assert_eq!(h.rom_size(), 512 * 1024);
    }
}
