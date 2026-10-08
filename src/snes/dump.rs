use std::io::Write;

use super::SFC;
use super::mapper::Mapper;
use crate::{Error, Result};

const ROM_CHUNK: usize = 0x8000;

pub struct Dumper<'a> {
    sfc: &'a mut SFC,
    mapper: Mapper,
    rom_size: usize,
}

impl<'a> Dumper<'a> {
    pub fn new(sfc: &'a mut SFC, mapper: Mapper, rom_size: usize) -> Self {
        Self {
            sfc,
            mapper,
            rom_size,
        }
    }

    /// `progress` is `(bytes_done, total)` after each chunk.
    pub fn dump(
        &mut self,
        w: &mut dyn Write,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<()> {
        if self.rom_size == 0 {
            return Err(Error::InvalidRomSize(self.rom_size));
        }
        let mut off = 0;
        while off < self.rom_size {
            self.before(off)?;
            let n = (self.rom_size - off).min(ROM_CHUNK);
            let addr = self.mapper.rom_addr(off);
            let b = self.sfc.read_bus(addr, n)?;
            if b.len() != n {
                return Err(Error::ShortRead {
                    addr,
                    got: b.len(),
                    want: n,
                });
            }
            if off + n == self.rom_size {
                // Issue the SPC7110 tail reset before the final file write,
                // matching the C (the tail writes happen in the read phase,
                // so the mapper is reset even if the final write fails).
                self.after_last()?;
            }
            w.write_all(&b)?;
            progress(off + n, self.rom_size);
            off += n;
        }
        Ok(())
    }

    fn before(&mut self, off: usize) -> Result<()> {
        match self.mapper {
            Mapper::SDD1 => {
                if off & 0xFFFFF == 0 {
                    self.sfc.write_bus_byte(0x4804, (off >> 20) as u8)?;
                }
            }
            Mapper::SPC7110 => {
                if off == 0x20_0000 {
                    self.sfc.write_bus_byte(0x4834, 0xFF)?;
                }
                if off == 0x40_0000 {
                    self.sfc.write_bus_byte(0x4833, 3)?;
                }
            }
            Mapper::CX4 => {
                if off == 0 {
                    let b = self.sfc.read_bus_byte(0x00FFC9)?;
                    // Match the C exactly (0x472ce0): only write $7F52 when
                    // the probe nibble is 2 (Mega Man X2) or 3 (Mega Man X3);
                    // skip it for any other value.
                    match b & 0xF {
                        2 => self.sfc.write_bus_byte(0x7F52, 0)?,
                        3 => self.sfc.write_bus_byte(0x7F52, 1)?,
                        _ => {}
                    }
                }
            }
            Mapper::Homebrew => {
                if off == 0 {
                    self.sfc.write_bus_byte(0x70009C, 7)?;
                    self.sfc.write_bus_byte(0x70009D, 0)?;
                    self.sfc.write_bus_byte(0x70009F, 0x12)?;
                }
                if off & 0x3F_FFFF == 0 {
                    self.sfc
                        .write_bus_byte(0x70009D, ((off >> 22) << 3) as u8)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn after_last(&mut self) -> Result<()> {
        if self.mapper != Mapper::SPC7110 {
            return Ok(());
        }
        self.sfc.write_bus_byte(0x4833, 2)?;
        self.sfc.write_bus_byte(0x4834, 0)
    }
}
