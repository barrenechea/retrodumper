use super::mapper::Mapper;

pub fn sram_addr(m: Mapper, off: usize) -> Option<usize> {
    Some(match m {
        // MMC 1, 4, 7
        Mapper::LoRom | Mapper::Derby96 | Mapper::CX4 => {
            0x70_0000 | ((off >> 15) << 16) | (off & 0x7FFF)
        }
        // MMC 2, 5, 6
        Mapper::HiRom | Mapper::SPC7110 | Mapper::SDD1 => {
            0x30_0000 | ((off >> 13) << 16) | 0x6000 | (off & 0x1FFF)
        }
        // MMC 3
        Mapper::ExHiRom => 0x80_0000 | ((off >> 13) << 16) | 0x6000 | (off & 0x1FFF),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srom_addr_mappings() {
        assert_eq!(sram_addr(Mapper::LoRom, 0x0000), Some(0x700000));
        assert_eq!(sram_addr(Mapper::LoRom, 0x1000), Some(0x701000));
        assert_eq!(sram_addr(Mapper::HiRom, 0x0000), Some(0x306000));
        assert_eq!(sram_addr(Mapper::ExHiRom, 0x0000), Some(0x806000));
        assert_eq!(sram_addr(Mapper::Homebrew, 0), None);
    }
}
