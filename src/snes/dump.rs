use std::io::Write;

use super::SFC;
use super::mapper::Mapper;
use crate::Result;
use crate::cart::DumpMap;

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
    pub fn dump(&mut self, w: &mut dyn Write, progress: impl FnMut(usize, usize)) -> Result<()> {
        crate::cart::dump_rom(self, w, progress)
    }
}

impl DumpMap for Dumper<'_> {
    fn chunk_size(&self) -> usize {
        ROM_CHUNK
    }

    fn rom_size(&self) -> usize {
        self.rom_size
    }

    fn address(&self, off: usize) -> usize {
        self.mapper.rom_addr(off)
    }

    fn read(&mut self, addr: usize, n: usize) -> Result<Vec<u8>> {
        self.sfc.read_bus(addr, n)
    }

    fn prepare(&mut self, off: usize) -> Result<()> {
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

    fn finish(&mut self) -> Result<()> {
        if self.mapper != Mapper::SPC7110 {
            return Ok(());
        }
        // Tail reset, issued in the client's read phase (before `dump_rom`
        // writes the last chunk).
        self.sfc.write_bus_byte(0x4833, 2)?;
        self.sfc.write_bus_byte(0x4834, 0)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;

    use super::*;
    use crate::device::{Bus, Info};
    use crate::protocol::{self, FRAME_SIZE};
    use crate::snes::SFC;

    struct Log {
        frames: Vec<Vec<u8>>,
        /// Frame count already recorded when each `DUMP.ROM` read starts.
        reads: Vec<(usize, usize, usize)>,
        /// Frame count already recorded when each file write starts.
        file_writes: Vec<usize>,
    }

    struct Fake {
        info: Info,
        log: Rc<RefCell<Log>>,
        byte: u8,
    }

    impl Bus for Fake {
        fn info(&self) -> &Info {
            &self.info
        }
        fn send(&mut self, frames: &[&[u8]]) -> Result<()> {
            let mut log = self.log.borrow_mut();
            for frame in frames {
                log.frames.push(frame.to_vec());
            }
            Ok(())
        }
        fn read_response(&mut self, n: usize) -> Result<Vec<u8>> {
            Ok(vec![self.byte; n])
        }
        fn read_bus(&mut self, addr: usize, n: usize) -> Result<Vec<u8>> {
            let seen = self.log.borrow().frames.len();
            self.log.borrow_mut().reads.push((addr, n, seen));
            Ok(vec![0xA5; n])
        }
    }

    struct LogWrite {
        log: Rc<RefCell<Log>>,
        buf: Vec<u8>,
    }

    impl Write for LogWrite {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let seen = self.log.borrow().frames.len();
            self.log.borrow_mut().file_writes.push(seen);
            self.buf.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn write_byte(frame: &[u8]) -> (usize, u8) {
        assert_eq!(frame.len(), FRAME_SIZE);
        assert_eq!(frame[0], protocol::MAGIC);
        assert_eq!(frame[1], crate::snes::GROUP_SFC);
        assert_eq!(frame[2], crate::snes::OP_SFC_WRITE_BYTE);
        let addr = u32::from_le_bytes([frame[3], frame[4], frame[5], 0]) as usize;
        (addr, frame[7])
    }

    #[test]
    fn spc7110_tail_reset_is_before_the_last_file_write() {
        let log = Rc::new(RefCell::new(Log {
            frames: Vec::new(),
            reads: Vec::new(),
            file_writes: Vec::new(),
        }));
        let mut sfc = SFC::new(Box::new(Fake {
            info: Info::default(),
            log: Rc::clone(&log),
            byte: 0,
        }));
        let rom_size = 0x40_8000;
        let mut dumper = Dumper::new(&mut sfc, Mapper::SPC7110, rom_size);
        let mut w = LogWrite {
            log: Rc::clone(&log),
            buf: Vec::new(),
        };
        let mut progress = Vec::new();
        dumper
            .dump(&mut w, |done, total| progress.push((done, total)))
            .unwrap();

        let log = log.borrow();
        assert_eq!(
            log.frames.iter().map(|f| write_byte(f)).collect::<Vec<_>>(),
            vec![(0x4834, 0xFF), (0x4833, 3), (0x4833, 2), (0x4834, 0)]
        );

        let mut off = 0;
        for (i, &(addr, n, frames_before)) in log.reads.iter().enumerate() {
            let len = (rom_size - off).min(ROM_CHUNK);
            assert_eq!((addr, n), (Mapper::SPC7110.rom_addr(off), len));
            let bank = if off < 0x20_0000 {
                0
            } else if off < 0x40_0000 {
                1
            } else {
                2
            };
            assert_eq!(frames_before, bank, "read {i} at {off:#X}");
            // The last chunk's file write sees the tail reset too.
            let file_frames = if off + len == rom_size { 4 } else { bank };
            assert_eq!(log.file_writes[i], file_frames);
            off += len;
        }
        assert_eq!(off, rom_size);
        assert_eq!(w.buf, vec![0xA5; rom_size]);
        assert_eq!(progress.last().copied(), Some((rom_size, rom_size)));
    }

    #[test]
    fn lorom_dump_sends_no_register_frames() {
        let log = Rc::new(RefCell::new(Log {
            frames: Vec::new(),
            reads: Vec::new(),
            file_writes: Vec::new(),
        }));
        let mut sfc = SFC::new(Box::new(Fake {
            info: Info::default(),
            log: Rc::clone(&log),
            byte: 0,
        }));
        let mut dumper = Dumper::new(&mut sfc, Mapper::LoRom, 0x1_0000);
        let mut w = LogWrite {
            log: Rc::clone(&log),
            buf: Vec::new(),
        };
        dumper.dump(&mut w, |_, _| {}).unwrap();
        let log = log.borrow();
        assert!(log.frames.is_empty());
        assert_eq!(
            log.reads
                .iter()
                .map(|&(a, n, _)| (a, n))
                .collect::<Vec<_>>(),
            vec![
                (Mapper::LoRom.rom_addr(0), 0x8000),
                (Mapper::LoRom.rom_addr(0x8000), 0x8000),
            ]
        );
    }
}
