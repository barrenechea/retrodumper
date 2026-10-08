pub mod carts;
pub mod dump;
pub mod header;
pub mod mapper;
pub mod sram;
pub mod verify;

use crate::device::{Bus, Info};
use crate::protocol::Frame;
use crate::{Error, Result};

pub use dump::Dumper;
pub use header::Header;
pub use mapper::{Mapper, detect_mapper};
pub use sram::sram_addr;
pub use verify::{apply_known_bad_cart_patch, rom_checksum, trimmed_sizes, verify_checksum};

pub const GROUP_SFC: u8 = 0x0F;

pub const OP_SFC_INIT: u8 = 0x01; // select SFC; IV[16]@0x0C, key[16]@0x1C
pub const OP_SFC_READ_BYTE: u8 = 0x02; // bus read; addr24@3, count@6
pub const OP_SFC_WRITE_BYTE: u8 = 0x03; // bus write; addr24@3, val@7
pub const OP_SFC_FLASH_WRITE: u8 = 0x05; // flash program write (fallback)
pub const OP_SFC_SRAM_READ: u8 = 0x09; // SRAM read; addr24@4, len16@8 (<=0x800)
pub const OP_SFC_SRAM_WRITE: u8 = 0x0A; // SRAM write; addr24@4, len16@8 (<=0x1F2)
pub const OP_SFC_FLASH_WRITE2: u8 = 0x0B; // flash program write (tried first)

pub(crate) const CRC32: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

/// $E000..$FFFF in DUMP.ROM.
const HEADER_ADDR: usize = 0xE000;
const HEADER_LENGTH: usize = 0x2000;

pub struct SFC {
    dev: Box<dyn Bus>,
    info: Info,
}

impl SFC {
    pub fn new(dev: Box<dyn Bus>) -> Self {
        let info = dev.info().clone();
        Self { dev, info }
    }

    /// Select the SFC system (command 0F 01). The client sends it before every
    /// Info, dump, SRAM and flash operation. The firmware spends ~300 ms
    /// inside that write.
    ///
    /// The key is ID0, ID1, ID0, ID1 as u32 LE at 0x1C. The IV is 16 random
    /// bytes at 0x0C; for SFC_L072_FW400 with firmware version 20231009 the
    /// IV is all zeros.
    pub fn init(&mut self) -> Result<()> {
        let mut f = Frame::new(GROUP_SFC, OP_SFC_INIT);
        let special = self.info.name == "SFC_L072_FW400" && self.info.version == 20231009;
        if !special {
            let mut iv = [0u8; 16];
            crate::device::sys::fill_random(&mut iv)?;
            f.set_bytes(0x0C, &iv);
        }
        let mut kb = [0u8; 16];
        let ids = [self.info.id0, self.info.id1, self.info.id0, self.info.id1];
        for (i, &v) in ids.iter().enumerate() {
            kb[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        f.set_bytes(0x1C, &kb);
        f.finish();
        let bytes: &[u8] = f.as_bytes();
        self.dev.send(&[bytes])
    }

    /// Read one byte from the SNES bus (command 0F 02). The response arrives
    /// in T-DRIVER.DMP at offset 0.
    pub fn read_bus_byte(&mut self, addr: usize) -> Result<u8> {
        let mut f = Frame::new(GROUP_SFC, OP_SFC_READ_BYTE);
        f.set_u24(3, addr as u32);
        f.set(6, 1);
        f.finish();
        let bytes: &[u8] = f.as_bytes();
        self.dev.send(&[bytes])?;
        let resp = self.dev.read_response(1)?;
        Ok(resp[0])
    }

    pub fn write_bus_byte(&mut self, addr: usize, v: u8) -> Result<()> {
        let mut f = Frame::new(GROUP_SFC, OP_SFC_WRITE_BYTE);
        f.set_u24(3, addr as u32);
        f.set(7, v);
        f.finish();
        let bytes: &[u8] = f.as_bytes();
        self.dev.send(&[bytes])
    }

    pub fn read_bus(&mut self, addr: usize, n: usize) -> Result<Vec<u8>> {
        self.dev.read_bus(addr, n)
    }

    pub fn info(&mut self) -> Result<Header> {
        self.init()?;
        let block = self.dev.read_bus(HEADER_ADDR, HEADER_LENGTH)?;
        if block.iter().all(|&c| c == 0xFF) {
            return Err(Error::NoCartridge);
        }
        let mut h = Header::parse(&block);
        // A table hit returns before the Homebrew probe (0x4735f0).
        if carts::lookup(h.block_crc).is_none() && h.homebrew {
            self.homebrew_probe(&block, &mut h)?;
        }
        Ok(h)
    }

    /// Reproduce the C's Homebrew Flashcard V1 info probe (0x4735f0).
    fn homebrew_probe(&mut self, first_block: &[u8], h: &mut Header) -> Result<()> {
        self.write_bus_byte(0x70009D, 1)?;
        // $70009F select: high nibble of bus 0xE03F (block 0x3F) from the
        // first read — 1 -> 0x12, 2 -> 0x22, otherwise 0x02.
        let sel = match first_block[0x3F] >> 4 {
            1 => 0x12,
            2 => 0x22,
            _ => 0x02,
        };
        self.write_bus_byte(0x70009F, sel)?;
        let mut block = self.dev.read_bus(HEADER_ADDR, HEADER_LENGTH)?;
        // Take the title from the game header (the C clears $FFD0 first).
        block[0x1FD0] = 0;
        h.title = header::title_for(&block);
        // Restore the window-select registers.
        self.write_bus_byte(0x70009D, 0)?;
        self.write_bus_byte(0x70009F, 0x12)?;
        // Force the size the C hardcodes for Homebrew V1.
        h.rom_override = 0x60_0000;
        h.sram_override = 0x8000;
        Ok(())
    }

    pub fn read_sram(&mut self, addr: usize, n: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(n);
        let mut off = 0;
        while off < n {
            let l = (n - off).min(0x800);
            let mut f = Frame::new(GROUP_SFC, OP_SFC_SRAM_READ);
            f.set_u24(4, (addr + off) as u32);
            f.set_u16(8, l as u16);
            f.finish();
            let bytes: &[u8] = f.as_bytes();
            self.dev.send(&[bytes])?;
            let resp = self.dev.read_response(l)?;
            out.extend_from_slice(&resp);
            off += l;
        }
        Ok(out)
    }

    pub fn write_sram(&mut self, addr: usize, data: &[u8]) -> Result<()> {
        let mut off = 0;
        while off < data.len() {
            let l = (data.len() - off).min(0x1F2);
            let mut f = Frame::new(GROUP_SFC, OP_SFC_SRAM_WRITE);
            f.set_u24(4, (addr + off) as u32);
            f.set_u16(8, l as u16);
            f.set_bytes(0x0C, &data[off..off + l]);
            f.finish();
            let bytes: &[u8] = f.as_bytes();
            self.dev.send(&[bytes])?;
            off += l;
        }
        Ok(())
    }

    pub fn read_save(&mut self, m: Mapper, n: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(n);
        let mut off = 0;
        while off < n {
            let l = (n - off).min(0x800);
            let addr = sram_addr(m, off).ok_or(Error::NoSram)?;
            let b = self.read_sram(addr, l)?;
            out.extend_from_slice(&b);
            off += l;
        }
        Ok(out)
    }

    pub fn write_save(&mut self, m: Mapper, data: &[u8], verify: bool) -> Result<()> {
        let mut off = 0;
        while off < data.len() {
            let l = (data.len() - off).min(0x1F2);
            let addr = sram_addr(m, off).ok_or(Error::NoSram)?;
            self.write_sram(addr, &data[off..off + l])?;
            off += l;
        }
        if verify {
            let back = self.read_save(m, data.len())?;
            if back != data {
                return Err(Error::VerifyFailed);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Frame, MAGIC};

    #[test]
    fn read_byte_frame_roundtrip() {
        let mut f = Frame::new(GROUP_SFC, OP_SFC_READ_BYTE);
        f.set_u24(3, 0x00FFC0);
        f.set(6, 1);
        f.finish();
        f.verify_crc().unwrap();
        assert_eq!(f.as_bytes()[0], MAGIC);
        assert_eq!(f.as_bytes()[1], GROUP_SFC);
        assert_eq!(f.as_bytes()[2], OP_SFC_READ_BYTE);
    }

    #[test]
    fn read_byte_frame_crc_rejects_corruption() {
        let mut f = Frame::new(GROUP_SFC, OP_SFC_READ_BYTE);
        f.finish();
        let bytes = f.as_bytes();
        let corrupted = {
            let mut g = f;
            g.set(10, bytes[10] ^ 0x01);
            g
        };
        assert!(matches!(
            corrupted.verify_crc(),
            Err(crate::Error::CrcMismatch { .. })
        ));
    }
}
