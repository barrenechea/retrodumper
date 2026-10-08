use super::header::Header;

/// An SFC ROM mapper (MMC) type, numbered as in the official client's MMC
/// setting table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Mapper {
    /// Plain LoROM (bank 0 at $008000, banks $00-$3F).
    LoRom = 1,
    /// Plain HiROM (bank 0 at $C00000).
    HiRom = 2,
    /// ExHiROM (HiROM plus the $400000-$7FFFFF window).
    ExHiRom = 3,
    /// The Derby Stallion 96 mapper: LoROM below 2 MB, banks $80+ above.
    Derby96 = 4,
    /// The SPC7110 3-chip mapper (8 MB ROM + SRAM).
    SPC7110 = 5,
    /// The SDD-1 32 Mbit mapper (bank $4804).
    SDD1 = 6,
    /// The Konami CX4 32 Mbit mapper.
    CX4 = 7,
    /// Homebrew Flashcard V1 simple. The client provides no SRAM mapping for
    /// it (mapper 8 has no case in the SRAM init, FUN_00473c90), so save
    /// read/write are unsupported.
    Homebrew = 8,
}

impl Mapper {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            1 => Some(Self::LoRom),
            2 => Some(Self::HiRom),
            3 => Some(Self::ExHiRom),
            4 => Some(Self::Derby96),
            5 => Some(Self::SPC7110),
            6 => Some(Self::SDD1),
            7 => Some(Self::CX4),
            8 => Some(Self::Homebrew),
            _ => None,
        }
    }

    pub fn number(self) -> u32 {
        self as u32
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::LoRom => "LOROM",
            Self::HiRom => "HIROM",
            Self::ExHiRom => "EXHIROM",
            Self::Derby96 => "Derby Stallion 96",
            Self::SPC7110 => "SPC7110",
            Self::SDD1 => "SDD-1",
            Self::CX4 => "CX4",
            Self::Homebrew => "Homebrew V1 Simple",
        }
    }

    pub fn rom_addr(self, off: usize) -> usize {
        match self {
            Self::LoRom | Self::CX4 => ((off >> 15) << 16) | 0x8000 | (off & 0x7FFF),
            Self::HiRom | Self::ExHiRom => {
                if off < 0x40_0000 {
                    off | 0xC0_0000
                } else {
                    (off & 0x3F_FFFF) | 0x40_0000
                }
            }
            Self::Derby96 => {
                if off < 0x20_0000 {
                    ((off >> 15) << 16) | 0x8000 | (off & 0x7FFF)
                } else {
                    let o = off - 0x20_0000;
                    0x80_0000 | ((o >> 15) << 16) | 0x8000 | (o & 0x7FFF)
                }
            }
            Self::SDD1 => 0xC0_0000 | (off & 0xFF_FFF),
            Self::SPC7110 => {
                if off < 0x20_0000 {
                    0xC0_0000 + off
                } else if off < 0x30_0000 {
                    0xE0_0000 + (off & 0xFF_FFF)
                } else {
                    0xF0_0000 + (off & 0xFF_FFF)
                }
            }
            Self::Homebrew => 0xC0_0000 | (off & 0x3F_FFFF),
        }
    }
}

/// Mapper suggested by the header. A cart-table hit and `--mmc` both win
/// over this.
///
/// - Homebrew Flashcard V1 simple reports the "MENU"/0xF8BB0744 signature
/// - SPC7110 reports map mode 0x3A (cartridge type 0xF5/0xF9)
/// - SDD-1 reports cartridge type 0x43/0x45
/// - CX4 reports cartridge type 0xF3
/// - ExHiROM reports map mode 0x25/0x35
/// - HiROM reports map mode 0x21/0x31
/// - everything else, including SA-1 and Super FX carts, is LoROM
pub fn detect_mapper(h: &Header) -> Mapper {
    if h.homebrew {
        return Mapper::Homebrew;
    }
    if h.map_mode == 0x3A {
        return Mapper::SPC7110;
    }
    if h.chip == 0x43 || h.chip == 0x45 {
        return Mapper::SDD1;
    }
    if h.chip == 0xF3 {
        return Mapper::CX4;
    }
    if h.map_mode & 0xEF == 0x25 {
        return Mapper::ExHiRom;
    }
    if h.map_mode & 0xEF == 0x21 {
        return Mapper::HiRom;
    }
    Mapper::LoRom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rom_addr_mappings() {
        // LoROM: banked at $008000.
        assert_eq!(Mapper::LoRom.rom_addr(0x00000), 0x008000);
        assert_eq!(Mapper::LoRom.rom_addr(0x00800), 0x008800);
        assert_eq!(Mapper::LoRom.rom_addr(0x08000), 0x018000);
        // HiROM: bank 0 at $C00000, wraps at 4 MB.
        assert_eq!(Mapper::HiRom.rom_addr(0x00000), 0xC00000);
        assert_eq!(Mapper::HiRom.rom_addr(0x3FFFF), 0xC3FFFF);
        assert_eq!(Mapper::HiRom.rom_addr(0x400000), 0x400000);
        // SDD1: window at $C00000.
        assert_eq!(Mapper::SDD1.rom_addr(0x12345), 0xC12345);
        // SPC7110: three windows.
        assert_eq!(Mapper::SPC7110.rom_addr(0x00000), 0xC00000);
        assert_eq!(Mapper::SPC7110.rom_addr(0x200000), 0xE00000);
        assert_eq!(Mapper::SPC7110.rom_addr(0x400000), 0xF00000);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn detect_by_map_mode() {
        let mut h = Header::default();
        h.map_mode = 0x21;
        assert_eq!(detect_mapper(&h), Mapper::HiRom);
        h.map_mode = 0x31;
        assert_eq!(detect_mapper(&h), Mapper::HiRom);
        h.map_mode = 0x25;
        assert_eq!(detect_mapper(&h), Mapper::ExHiRom);
        h.map_mode = 0x3A;
        assert_eq!(detect_mapper(&h), Mapper::SPC7110);
        h.map_mode = 0x20;
        h.chip = 0xF3;
        assert_eq!(detect_mapper(&h), Mapper::CX4);
        h.chip = 0x43;
        assert_eq!(detect_mapper(&h), Mapper::SDD1);
        h.chip = 0x02;
        assert_eq!(detect_mapper(&h), Mapper::LoRom);
    }

    #[test]
    fn from_u32_roundtrip() {
        for v in 1..=8u32 {
            let m = Mapper::from_u32(v).unwrap();
            assert_eq!(m.number(), v);
        }
        assert!(Mapper::from_u32(0).is_none());
        assert!(Mapper::from_u32(9).is_none());
    }
}
