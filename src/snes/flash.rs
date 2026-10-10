//! SFC flash programming (`0x473b40` init, `0x472f70` dispatcher).
//!
//! Single-byte unlock writes use opcode `0x0B`, then `0x05` if CFI size is 0.
//! Homebrew uses opcode `0x03` for those writes. Bulk data is opcodes `0x06`,
//! `0x07`, `0x0C`, or `0x0D`. SRAM stays `0x0A` and does not enter this path.

use std::borrow::Cow;

use super::mapper::Mapper;
use super::{
    OP_SFC_FLASH_PROG_0C, OP_SFC_FLASH_PROG_0D, OP_SFC_FLASH_PROG_06, OP_SFC_FLASH_PROG_07,
    OP_SFC_FLASH_WRITE, OP_SFC_FLASH_WRITE2, OP_SFC_WRITE_BYTE, SFC,
};
use crate::Result;
use crate::cart::{EraseStep, Program};
use crate::protocol::Frame;

const CFI_ADDR: usize = 0x8000;
const CFI_LEN: usize = 0x400;
const WINDOW_LO: usize = 0x8000;
const WINDOW_HI: usize = 0x10000;
const SECTOR: usize = 0x20000;
/// `cmp esi, 0xC350` / `jg` (`0x472f70`). Extra reads run while the counter
/// is still `<= 0xC350`, which is `0xC351` further reads. A miss is `0xC352`.
const SECTOR_POLLS: u32 = 0xC350;

const ID_0101: u32 = 0x7E7E_0101;
const ID_C2C2: u32 = 0x7E7E_C2C2;

/// `0x473b40`: mappers 1/4/7 are mode 0, 2/3/5/6 are mode 1, Homebrew is mode 2.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FlashMode {
    /// 0x8000-byte windows, chip erase.
    Lo,
    /// 0x10000-byte windows, chip erase.
    Hi,
    /// 0x10000-byte windows. Large chips erase by sector.
    Homebrew,
}

fn flash_layout(mapper: Mapper) -> (FlashMode, usize) {
    match mapper {
        Mapper::LoRom | Mapper::Derby96 | Mapper::CX4 => (FlashMode::Lo, WINDOW_LO),
        Mapper::HiRom | Mapper::ExHiRom | Mapper::SPC7110 | Mapper::SDD1 => {
            (FlashMode::Hi, WINDOW_HI)
        }
        Mapper::Homebrew => (FlashMode::Homebrew, WINDOW_HI),
    }
}

fn mode0_addr(off: usize) -> usize {
    (((off & 0xFF_8000) | 0x4000) * 2) | (off & 0x7FFF)
}

fn mode1_addr(off: usize) -> usize {
    if off < 0x40_0000 {
        off | 0xC0_0000
    } else {
        (off & 0x3F_FFFF) | 0x40_0000
    }
}

/// `FUN_0046bf00`. Odd `QRY` wins over even. Codes `0x13`..=`0x1C` are sizes.
fn cfi_size(buf: &[u8]) -> usize {
    let code = if buf.len() > 0x4F && buf[0x21] == b'Q' && buf[0x23] == b'R' && buf[0x25] == b'Y' {
        buf[0x4F]
    } else if buf.len() > 0x4E && buf[0x20] == b'Q' && buf[0x22] == b'R' && buf[0x24] == b'Y' {
        buf[0x4E]
    } else {
        return 0;
    };
    match code {
        0x13 => 0x8_0000,
        0x14 => 0x10_0000,
        0x15 => 0x20_0000,
        0x16 => 0x40_0000,
        0x17 => 0x80_0000,
        0x18 => 0x100_0000,
        0x19 => 0x200_0000,
        0x1A => 0x400_0000,
        0x1B => 0x800_0000,
        0x1C => 0x1000_0000,
        _ => 0,
    }
}

/// Chip larger than 8 MB and `file_len * 2` still fits (`0x472f70`, signed
/// compare with `0x800000`). The dispatcher only reaches this from mode 2.
fn use_sector_erase(flash_size: usize, image_len: usize) -> bool {
    let size = flash_size as u32;
    if (size as i32) <= 0x80_0000 {
        return false;
    }
    (image_len as u32).wrapping_mul(2) <= size
}

fn window_bytes(data: &[u8], window: usize) -> Vec<u8> {
    let mut out = vec![0xFF; window];
    let n = data.len().min(window);
    out[..n].copy_from_slice(&data[..n]);
    out
}

pub struct Flash<'a> {
    sfc: &'a mut SFC,
    mode: FlashMode,
    window: usize,
    /// Opcode for unlock / CFI / ID bytes. `0x0B`, `0x05`, or Homebrew `0x03`.
    writer: u8,
    prog_op: u8,
    frame_len: usize,
    flash_size: usize,
    erase_at: usize,
    erase_cmd_sent: bool,
}

impl<'a> Flash<'a> {
    pub fn new(sfc: &'a mut SFC, mapper: Mapper) -> Self {
        let (mode, window) = flash_layout(mapper);
        Self {
            sfc,
            mode,
            window,
            writer: OP_SFC_FLASH_WRITE2,
            prog_op: OP_SFC_FLASH_PROG_0C,
            frame_len: 0x1F2,
            flash_size: 0,
            erase_at: 0,
            erase_cmd_sent: false,
        }
    }

    fn cmd(&mut self, addr: usize, val: u8) -> Result<()> {
        self.sfc.write_opcode_byte(self.writer, addr, val)
    }

    fn probe_cfi(&mut self) -> Result<()> {
        self.cmd(0x80AA, 0x98)?;
        let buf = self.sfc.read_bus(CFI_ADDR, CFI_LEN)?;
        self.cmd(CFI_ADDR, 0xF0)?;
        self.flash_size = cfi_size(&buf);
        Ok(())
    }

    fn read_id(&mut self) -> Result<u32> {
        self.cmd(0x8AAA, 0xAA)?;
        self.cmd(0x8555, 0x55)?;
        self.cmd(0x8AAA, 0x90)?;
        let buf = self.sfc.read_bus(CFI_ADDR, CFI_LEN)?;
        self.cmd(0x8AAA, 0xF0)?;
        let mut b = [0u8; 4];
        if buf.len() >= 4 {
            b.copy_from_slice(&buf[..4]);
        }
        Ok(u32::from_le_bytes(b))
    }

    /// `0x7E7E0101` and `0x7E7EC2C2` pick the `0x1E0` opcode (`0x0D`, or
    /// `0x07` when `narrow`). Any other ID picks `0x1F2` (`0x0C`, or `0x06`
    /// when `narrow`).
    fn select_program(&mut self, narrow: bool, id: u32) {
        let special = id == ID_0101 || id == ID_C2C2;
        let (op, frame_len) = match (narrow, special) {
            (false, true) => (OP_SFC_FLASH_PROG_0D, 0x1E0),
            (false, false) => (OP_SFC_FLASH_PROG_0C, 0x1F2),
            (true, true) => (OP_SFC_FLASH_PROG_07, 0x1E0),
            (true, false) => (OP_SFC_FLASH_PROG_06, 0x1F2),
        };
        self.prog_op = op;
        self.frame_len = frame_len;
    }

    fn unlock(&mut self, addr_aa: usize, addr_55: usize, cmd_addr: usize, cmd: u8) -> Result<()> {
        self.cmd(addr_aa, 0xAA)?;
        self.cmd(addr_55, 0x55)?;
        self.cmd(addr_aa, 0x80)?;
        self.cmd(addr_aa, 0xAA)?;
        self.cmd(addr_55, 0x55)?;
        self.cmd(cmd_addr, cmd)
    }

    fn erase_chip(&mut self) -> Result<EraseStep> {
        if !self.erase_cmd_sent {
            self.unlock(0x8AAA, 0x8555, 0x8AAA, 0x10)?;
            self.erase_cmd_sent = true;
            return Ok(EraseStep::Pending);
        }
        let buf = self.sfc.read_bus(CFI_ADDR, CFI_LEN)?;
        if buf.first().copied() == Some(0xFF) {
            Ok(EraseStep::Done)
        } else {
            Ok(EraseStep::Wait)
        }
    }

    fn poll_sector(&mut self, addr: usize) -> Result<()> {
        let buf = self.sfc.read_bus(addr, CFI_LEN)?;
        if buf.first().copied() == Some(0xFF) {
            return Ok(());
        }
        // One read plus at most 0xC351 more.
        for _ in 0..=SECTOR_POLLS {
            let buf = self.sfc.read_bus(addr, CFI_LEN)?;
            if buf.first().copied() == Some(0xFF) {
                break;
            }
        }
        Ok(())
    }

    fn erase_sector(&mut self, image_len: usize) -> Result<EraseStep> {
        if self.erase_at >= image_len {
            return Ok(EraseStep::Done);
        }
        let old = self.erase_at;
        self.erase_at = old.saturating_add(SECTOR);
        if old & 0x3F_FFFF == 0 {
            self.sfc
                .write_bus_byte(0x70009D, ((old >> 22) << 3) as u8)?;
        }
        let dest = (old & 0x3F_FFFF) | 0xC0_0000;
        self.unlock(0xC00AAA, 0xC00555, dest, 0x30)?;
        // A sector that never reads 0xFF still advances. `0x472f70` returns
        // pending after the poll either way, and the next call moves on.
        self.poll_sector(dest)?;
        Ok(EraseStep::Pending)
    }

    fn send_bulk(&mut self, mut addr: usize, data: &[u8]) -> Result<()> {
        let mut owned = Vec::new();
        let mut off = 0;
        while off < data.len() {
            let n = (data.len() - off).min(self.frame_len);
            let mut f = Frame::new(super::GROUP_SFC, self.prog_op);
            // Plaintext 0x5A, same as the unlock commands (`0x45dbd0` forces
            // that magic). The bulk sender `0x45ddc0` stamps 0x5B after SNOW.
            f.set_u32(4, addr as u32);
            f.set_u16(8, n as u16);
            f.set_bytes(0x0C, &data[off..off + n]);
            f.finish();
            owned.push(*f.as_bytes());
            addr += n;
            off += n;
        }
        if owned.is_empty() {
            return Ok(());
        }
        let refs: Vec<&[u8]> = owned.iter().map(|f| f.as_slice()).collect();
        self.sfc.send_frames(&refs)
    }
}

impl Program for Flash<'_> {
    fn chunk_size(&self) -> usize {
        self.window
    }

    fn prefix_len(&self) -> usize {
        if self.mode == FlashMode::Homebrew {
            0x8_0000
        } else {
            0
        }
    }

    fn image_to_write<'b>(&mut self, file: &'b [u8]) -> Cow<'b, [u8]> {
        if self.mode == FlashMode::Homebrew {
            Cow::Owned(super::homebrew::image(file))
        } else {
            Cow::Borrowed(file)
        }
    }

    fn identify(&mut self) -> Result<usize> {
        self.erase_at = 0;
        self.erase_cmd_sent = false;
        self.sfc.init()?;
        if self.mode == FlashMode::Homebrew {
            self.sfc.write_bus_byte(0x70009C, 7)?;
            self.sfc.write_bus_byte(0x70009D, 0)?;
            self.sfc.write_bus_byte(0x70009F, 0x12)?;
            self.writer = OP_SFC_WRITE_BYTE;
            self.probe_cfi()?;
            let id = self.read_id()?;
            self.select_program(true, id);
            return Ok(self.flash_size);
        }
        self.writer = OP_SFC_FLASH_WRITE2;
        self.probe_cfi()?;
        if self.flash_size == 0 {
            self.writer = OP_SFC_FLASH_WRITE;
            self.probe_cfi()?;
            if self.flash_size == 0 {
                return Ok(0);
            }
        }
        let id = self.read_id()?;
        self.select_program(self.writer == OP_SFC_FLASH_WRITE, id);
        Ok(self.flash_size)
    }

    fn erase_step(&mut self, image_len: usize) -> Result<EraseStep> {
        if self.mode == FlashMode::Homebrew && use_sector_erase(self.flash_size, image_len) {
            self.erase_sector(image_len)
        } else {
            self.erase_chip()
        }
    }

    fn program_chunk(&mut self, off: usize, data: &[u8]) -> Result<()> {
        // Mode 0/1 always submit a full window (`0x472f70` pushes 0x8000
        // or 0x10000). A short tail is filled with 0xFF.
        match self.mode {
            FlashMode::Lo => self.send_bulk(mode0_addr(off), &window_bytes(data, WINDOW_LO)),
            FlashMode::Hi => self.send_bulk(mode1_addr(off), &window_bytes(data, WINDOW_HI)),
            FlashMode::Homebrew => {
                if off & 0x3F_FFFF == 0 {
                    self.sfc
                        .write_bus_byte(0x70009D, ((off >> 22) << 3) as u8)?;
                }
                self.send_bulk((off & 0x3F_FFFF) | 0xC0_0000, data)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;
    use crate::cart::program_image;
    use crate::device::{Bus, Info};
    use crate::protocol::{FRAME_SIZE, MAGIC};
    use crate::snes::SFC;

    struct Log {
        frames: Vec<Vec<u8>>,
        reads: VecDeque<Vec<u8>>,
        /// `(addr, len, frames already sent)` at each `DUMP.ROM` read.
        bus: Vec<(usize, usize, usize)>,
        /// How many frames each `send` contained.
        sends: Vec<usize>,
        fill: u8,
    }

    struct Fake {
        info: Info,
        log: Rc<RefCell<Log>>,
    }

    impl Bus for Fake {
        fn info(&self) -> &Info {
            &self.info
        }
        fn send(&mut self, frames: &[&[u8]]) -> Result<()> {
            let mut log = self.log.borrow_mut();
            log.sends.push(frames.len());
            for frame in frames {
                log.frames.push(frame.to_vec());
            }
            Ok(())
        }
        fn read_response(&mut self, n: usize) -> Result<Vec<u8>> {
            Ok(vec![0; n])
        }
        fn read_bus(&mut self, addr: usize, n: usize) -> Result<Vec<u8>> {
            let mut log = self.log.borrow_mut();
            let seen = log.frames.len();
            log.bus.push((addr, n, seen));
            let fill = log.fill;
            Ok(log.reads.pop_front().unwrap_or_else(|| vec![fill; n]))
        }
    }

    fn harness(reads: Vec<Vec<u8>>) -> (SFC, Rc<RefCell<Log>>) {
        let log = Rc::new(RefCell::new(Log {
            frames: Vec::new(),
            reads: VecDeque::from(reads),
            bus: Vec::new(),
            sends: Vec::new(),
            fill: 0xFF,
        }));
        let fake = Fake {
            info: Info {
                name: "SFC_L072_FW400".into(),
                version: 20231009,
                ..Info::default()
            },
            log: Rc::clone(&log),
        };
        (SFC::new(Box::new(fake)), log)
    }

    fn cfi_buf(code: u8, odd: bool) -> Vec<u8> {
        let mut b = vec![0; CFI_LEN];
        if odd {
            b[0x21] = b'Q';
            b[0x23] = b'R';
            b[0x25] = b'Y';
            b[0x4F] = code;
        } else {
            b[0x20] = b'Q';
            b[0x22] = b'R';
            b[0x24] = b'Y';
            b[0x4E] = code;
        }
        b
    }

    fn id_buf(id: u32) -> Vec<u8> {
        let mut b = vec![0; CFI_LEN];
        b[..4].copy_from_slice(&id.to_le_bytes());
        b
    }

    fn byte_cmds(frames: &[Vec<u8>]) -> Vec<(u8, usize, u8)> {
        frames
            .iter()
            .filter_map(|f| {
                assert_eq!(f[0], MAGIC);
                assert_eq!(f[1], super::super::GROUP_SFC);
                match f[2] {
                    0x03 | 0x05 | 0x0B => {
                        let addr = usize::from(f[3])
                            | (usize::from(f[4]) << 8)
                            | (usize::from(f[5]) << 16);
                        assert_eq!(f[6], 0);
                        Some((f[2], addr, f[7]))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    fn assert_plain_frame(f: &[u8]) {
        assert_eq!(f.len(), FRAME_SIZE);
        assert_eq!(f[0], MAGIC);
        assert_eq!(f[1], super::super::GROUP_SFC);
        let crc = crc::Crc::<u16>::new(&crc::CRC_16_XMODEM).checksum(&f[..0x1FE]);
        assert_eq!([f[0x1FE], f[0x1FF]], [(crc >> 8) as u8, crc as u8]);
    }

    #[test]
    fn cfi_size_prefers_odd_qry() {
        assert_eq!(cfi_size(&[]), 0);
        assert_eq!(cfi_size(&cfi_buf(0x12, false)), 0);
        let codes = [
            (0x13u8, 0x8_0000usize),
            (0x14, 0x10_0000),
            (0x15, 0x20_0000),
            (0x16, 0x40_0000),
            (0x17, 0x80_0000),
            (0x18, 0x100_0000),
            (0x19, 0x200_0000),
            (0x1A, 0x400_0000),
            (0x1B, 0x800_0000),
            (0x1C, 0x1000_0000),
        ];
        for (code, size) in codes {
            assert_eq!(cfi_size(&cfi_buf(code, false)), size);
        }
        assert_eq!(cfi_size(&cfi_buf(0x1C, true)), 0x1000_0000);
        let mut both = cfi_buf(0x14, false);
        both[0x21] = b'Q';
        both[0x23] = b'R';
        both[0x25] = b'Y';
        both[0x4F] = 0x16;
        assert_eq!(cfi_size(&both), 0x40_0000);
    }

    #[test]
    fn sector_erase_is_only_the_large_homebrew_case() {
        assert!(!use_sector_erase(0x80_0000, 1));
        assert!(use_sector_erase(0x80_0001, 1));
        assert!(use_sector_erase(0x100_0000, 0x80_0000));
        assert!(!use_sector_erase(0x100_0000, 0x80_0001));
        // `image_len as u32` wraps, so `* 2` is 0 and the sector path is taken.
        assert!(use_sector_erase(0x100_0000, 0x8000_0000));
        assert_eq!(mode0_addr(0), 0x8000);
        assert_eq!(mode0_addr(0x8000), 0x1_8000);
        assert_eq!(mode1_addr(0), 0xC0_0000);
        assert_eq!(mode1_addr(0x40_0000), 0x40_0000);
    }

    #[test]
    fn lorom_identify_uses_0b_then_id() {
        let (mut sfc, log) = harness(vec![cfi_buf(0x16, false), id_buf(ID_0101)]);
        let mut flash = Flash::new(&mut sfc, Mapper::LoRom);
        assert_eq!(flash.identify().unwrap(), 0x40_0000);
        let frames = log.borrow().frames.clone();
        assert!(frames.iter().all(|f| f[2] != 0x0A && f[2] != 0x09));
        assert!(frames.iter().all(|f| f[2] != 0x05));
        assert_eq!(
            byte_cmds(&frames),
            vec![
                (0x0B, 0x80AA, 0x98),
                (0x0B, 0x8000, 0xF0),
                (0x0B, 0x8AAA, 0xAA),
                (0x0B, 0x8555, 0x55),
                (0x0B, 0x8AAA, 0x90),
                (0x0B, 0x8AAA, 0xF0),
            ]
        );
        let bus = log.borrow().bus.clone();
        assert_eq!(bus, vec![(0x8000, 0x400, 2), (0x8000, 0x400, 6)]);
        let cfi_at = bus[0].2;
        let id_at = bus[1].2;
        assert_eq!(
            byte_cmds(&frames[..cfi_at]).last().copied().map(|c| c.2),
            Some(0x98)
        );
        assert_eq!(
            byte_cmds(&frames[cfi_at..id_at]).first().copied(),
            Some((0x0B, 0x8000, 0xF0))
        );
        assert_eq!(
            byte_cmds(&frames[..id_at]).last().copied().map(|c| c.2),
            Some(0x90)
        );
        assert_eq!(
            byte_cmds(&frames[id_at..]).first().copied(),
            Some((0x0B, 0x8AAA, 0xF0))
        );
    }

    #[test]
    fn cfi_miss_on_0b_retries_with_05() {
        let (mut sfc, log) = harness(vec![
            vec![0; CFI_LEN],
            cfi_buf(0x16, false),
            id_buf(ID_C2C2),
        ]);
        let mut flash = Flash::new(&mut sfc, Mapper::HiRom);
        assert_eq!(flash.identify().unwrap(), 0x40_0000);
        assert_eq!(
            byte_cmds(&log.borrow().frames),
            vec![
                (0x0B, 0x80AA, 0x98),
                (0x0B, 0x8000, 0xF0),
                (0x05, 0x80AA, 0x98),
                (0x05, 0x8000, 0xF0),
                (0x05, 0x8AAA, 0xAA),
                (0x05, 0x8555, 0x55),
                (0x05, 0x8AAA, 0x90),
                (0x05, 0x8AAA, 0xF0),
            ]
        );
    }

    #[test]
    fn both_cfi_misses_skip_the_id_command() {
        let (mut sfc, log) = harness(vec![vec![0; CFI_LEN], vec![0; CFI_LEN]]);
        let mut flash = Flash::new(&mut sfc, Mapper::LoRom);
        assert_eq!(flash.identify().unwrap(), 0);
        let cmds = byte_cmds(&log.borrow().frames);
        assert_eq!(cmds.len(), 4);
        assert!(cmds.iter().all(|c| c.1 != 0x8AAA));
    }

    #[test]
    fn homebrew_identify_uses_opcode_03() {
        let (mut sfc, log) = harness(vec![cfi_buf(0x18, false), id_buf(ID_0101)]);
        let mut flash = Flash::new(&mut sfc, Mapper::Homebrew);
        assert_eq!(flash.identify().unwrap(), 0x100_0000);
        assert_eq!(
            byte_cmds(&log.borrow().frames),
            vec![
                (0x03, 0x70009C, 7),
                (0x03, 0x70009D, 0),
                (0x03, 0x70009F, 0x12),
                (0x03, 0x80AA, 0x98),
                (0x03, 0x8000, 0xF0),
                (0x03, 0x8AAA, 0xAA),
                (0x03, 0x8555, 0x55),
                (0x03, 0x8AAA, 0x90),
                (0x03, 0x8AAA, 0xF0),
            ]
        );
    }

    #[test]
    fn chip_erase_then_poll() {
        let (mut sfc, log) = harness(vec![
            cfi_buf(0x16, false),
            id_buf(ID_0101),
            vec![0; CFI_LEN],
        ]);
        let mut flash = Flash::new(&mut sfc, Mapper::CX4);
        flash.identify().unwrap();
        let before = log.borrow().frames.len();
        assert_eq!(flash.erase_step(0x100).unwrap(), EraseStep::Pending);
        assert_eq!(
            byte_cmds(&log.borrow().frames[before..]),
            vec![
                (0x0B, 0x8AAA, 0xAA),
                (0x0B, 0x8555, 0x55),
                (0x0B, 0x8AAA, 0x80),
                (0x0B, 0x8AAA, 0xAA),
                (0x0B, 0x8555, 0x55),
                (0x0B, 0x8AAA, 0x10),
            ]
        );
        assert_eq!(flash.erase_step(0x100).unwrap(), EraseStep::Wait);
        assert_eq!(flash.erase_step(0x100).unwrap(), EraseStep::Done);
        let bus = log.borrow().bus.clone();
        assert_eq!(bus[bus.len() - 2].0, 0x8000);
        assert_eq!(bus[bus.len() - 1].0, 0x8000);
    }

    #[test]
    fn eight_meg_homebrew_chip_erases_not_sectors() {
        let (mut sfc, log) = harness(vec![cfi_buf(0x17, false), id_buf(0)]);
        let mut flash = Flash::new(&mut sfc, Mapper::Homebrew);
        flash.identify().unwrap();
        let before = log.borrow().frames.len();
        assert_eq!(flash.erase_step(0x1000).unwrap(), EraseStep::Pending);
        let cmds = byte_cmds(&log.borrow().frames[before..]);
        assert_eq!(cmds.last().copied(), Some((0x03, 0x8AAA, 0x10)));
        assert!(cmds.iter().all(|c| c.1 != 0xC0_0000));
    }

    #[test]
    fn large_homebrew_erases_one_sector_per_step() {
        let (mut sfc, log) = harness(vec![cfi_buf(0x18, false), id_buf(0)]);
        let mut flash = Flash::new(&mut sfc, Mapper::Homebrew);
        flash.identify().unwrap();
        let before = log.borrow().frames.len();
        let before_bus = log.borrow().bus.len();
        assert_eq!(flash.erase_step(0x2_0000).unwrap(), EraseStep::Pending);
        assert_eq!(
            byte_cmds(&log.borrow().frames[before..]),
            vec![
                (0x03, 0x70009D, 0),
                (0x03, 0xC00AAA, 0xAA),
                (0x03, 0xC00555, 0x55),
                (0x03, 0xC00AAA, 0x80),
                (0x03, 0xC00AAA, 0xAA),
                (0x03, 0xC00555, 0x55),
                (0x03, 0xC0_0000, 0x30),
            ]
        );
        assert_eq!(
            log.borrow().bus.last().map(|b| (b.0, b.1)),
            Some((0xC0_0000, 0x400))
        );
        assert_eq!(log.borrow().bus.len() - before_bus, 1);
        let mid = log.borrow().frames.len();
        assert_eq!(flash.erase_step(0x2_0000).unwrap(), EraseStep::Done);
        assert_eq!(log.borrow().frames.len(), mid);

        assert_eq!(flash.erase_step(0x4_0000).unwrap(), EraseStep::Pending);
        let added = byte_cmds(&log.borrow().frames[mid..]);
        assert_eq!(added.last().copied(), Some((0x03, 0xC2_0000, 0x30)));
        assert!(added.iter().all(|c| c.1 != 0x70009D));
    }

    #[test]
    fn bulk_program_frame_and_opcode_follow_the_id() {
        let (mut sfc, log) = harness(vec![cfi_buf(0x18, false), id_buf(ID_0101)]);
        let mut flash = Flash::new(&mut sfc, Mapper::Homebrew);
        flash.identify().unwrap();
        log.borrow_mut().frames.clear();
        flash.program_chunk(0, &[0xAB, 0xCD]).unwrap();
        let frames = log.borrow().frames.clone();
        assert_eq!(frames[0][2], 0x03);
        assert_eq!(frames[0][7], 0);
        let bulk = &frames[1];
        assert_plain_frame(bulk);
        assert_eq!(bulk[2], OP_SFC_FLASH_PROG_07);
        assert_eq!(
            u32::from_le_bytes(bulk[4..8].try_into().unwrap()),
            0xC0_0000
        );
        assert_eq!(u16::from_le_bytes(bulk[8..10].try_into().unwrap()), 2);
        assert_eq!(&bulk[10..12], &[0, 0]);
        assert_eq!(&bulk[0x0C..0x0E], &[0xAB, 0xCD]);

        log.borrow_mut().frames.clear();
        flash.program_chunk(0x1000, &[0x11]).unwrap();
        let frames = log.borrow().frames.clone();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            u32::from_le_bytes(frames[0][4..8].try_into().unwrap()),
            0xC0_1000
        );

        log.borrow_mut().frames.clear();
        flash.program_chunk(0x40_0000, &[0x22]).unwrap();
        let frames = log.borrow().frames.clone();
        assert_eq!(frames[0][2], 0x03);
        assert_eq!(frames[0][7], 8);
        assert_eq!(
            u32::from_le_bytes(frames[1][4..8].try_into().unwrap()),
            0xC0_0000
        );
    }

    #[test]
    fn mode0_pads_a_short_tail_and_picks_0d_or_0c() {
        let (mut sfc, log) = harness(vec![cfi_buf(0x16, false), id_buf(ID_0101)]);
        let mut flash = Flash::new(&mut sfc, Mapper::Derby96);
        flash.identify().unwrap();
        log.borrow_mut().frames.clear();
        let sends_before = log.borrow().sends.len();
        flash.program_chunk(0x8000, &[0x11]).unwrap();
        assert_eq!(&log.borrow().sends[sends_before..], &[69]);
        let frames = log.borrow().frames.clone();
        for f in &frames {
            assert_plain_frame(f);
        }
        assert_eq!(frames.len(), 69);
        assert!(frames.iter().all(|f| f[2] == OP_SFC_FLASH_PROG_0D));
        let mut addr = 0x1_8000u32;
        for f in &frames {
            assert_eq!(u32::from_le_bytes(f[4..8].try_into().unwrap()), addr);
            addr += u32::from(u16::from_le_bytes(f[8..10].try_into().unwrap()));
        }
        assert_eq!(addr, 0x1_8000 + WINDOW_LO as u32);
        assert_eq!(
            u16::from_le_bytes(frames[0][8..10].try_into().unwrap()),
            0x1E0
        );
        assert_eq!(frames[0][0x0C], 0x11);
        assert_eq!(frames[0][0x0D], 0xFF);
        assert_eq!(
            u16::from_le_bytes(frames[68][8..10].try_into().unwrap()),
            0x80
        );
        let mut total = 0usize;
        for f in &frames {
            total += usize::from(u16::from_le_bytes(f[8..10].try_into().unwrap()));
        }
        assert_eq!(total, WINDOW_LO);

        let (mut sfc, log) = harness(vec![cfi_buf(0x16, false), id_buf(0x1111_1111)]);
        let mut flash = Flash::new(&mut sfc, Mapper::LoRom);
        flash.identify().unwrap();
        log.borrow_mut().frames.clear();
        flash.program_chunk(0, &[0x22]).unwrap();
        let frames = log.borrow().frames.clone();
        assert_eq!(frames[0][2], OP_SFC_FLASH_PROG_0C);
        assert_eq!(
            u16::from_le_bytes(frames[0][8..10].try_into().unwrap()),
            0x1F2
        );
        assert_eq!(
            u32::from_le_bytes(frames[0][4..8].try_into().unwrap()),
            0x8000
        );
    }

    #[test]
    fn mode1_window_starts_at_the_hirom_address() {
        let (mut sfc, log) = harness(vec![
            vec![0; CFI_LEN],
            cfi_buf(0x16, false),
            id_buf(ID_0101),
        ]);
        let mut flash = Flash::new(&mut sfc, Mapper::HiRom);
        flash.identify().unwrap();
        log.borrow_mut().frames.clear();
        flash.program_chunk(0, &[0x33]).unwrap();
        let frames = log.borrow().frames.clone();
        assert_eq!(frames[0][2], OP_SFC_FLASH_PROG_07);
        assert_eq!(
            u32::from_le_bytes(frames[0][4..8].try_into().unwrap()),
            0xC0_0000
        );
        let mut total = 0usize;
        for f in &frames {
            total += usize::from(u16::from_le_bytes(f[8..10].try_into().unwrap()));
        }
        assert_eq!(total, WINDOW_HI);
        assert_eq!(frames[0][0x0C], 0x33);
        assert_eq!(frames[0][0x0D], 0xFF);
    }

    #[test]
    fn program_image_erases_before_the_bulk_write() {
        let (mut sfc, log) = harness(vec![cfi_buf(0x16, false), id_buf(ID_C2C2)]);
        let mut flash = Flash::new(&mut sfc, Mapper::LoRom);
        program_image(
            &mut flash,
            &[0x5A],
            |_| {},
            || {},
            || {},
            |done, total| {
                assert_eq!(total, 1);
                assert_eq!(done, 1);
            },
        )
        .unwrap();
        let frames = log.borrow().frames.clone();
        let erase = frames
            .iter()
            .position(|f| f[2] == 0x0B && f[7] == 0x10)
            .unwrap();
        let bulk = frames
            .iter()
            .position(|f| f[2] == OP_SFC_FLASH_PROG_0D)
            .unwrap();
        assert!(erase < bulk);
        assert_eq!(frames[bulk][0x0C], 0x5A);
        assert!(frames.iter().all(|f| f[2] != 0x0A));
    }

    #[test]
    fn homebrew_file_must_fit_with_the_menu() {
        let (mut sfc, log) = harness(vec![cfi_buf(0x13, false), id_buf(0)]);
        let mut flash = Flash::new(&mut sfc, Mapper::Homebrew);
        let mut sized = false;
        let mut erased = false;
        let err = program_image(
            &mut flash,
            &[0x11],
            |_| sized = true,
            || erased = true,
            || {},
            |_, _| {},
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "snes: ROM file size error.");
        assert!(sized);
        assert!(!erased);
        assert!(
            byte_cmds(&log.borrow().frames)
                .iter()
                .all(|c| c.2 != 0x10 && c.2 != 0x30)
        );
    }

    #[test]
    fn homebrew_programs_the_menu_image() {
        let (mut sfc, log) = harness(vec![cfi_buf(0x14, false), id_buf(0)]);
        let mut flash = Flash::new(&mut sfc, Mapper::Homebrew);
        let file = [0x11u8];
        let expect = super::super::homebrew::image(&file);
        let mut erased = false;
        let written = program_image(
            &mut flash,
            &file,
            |_| {},
            || erased = true,
            || {},
            |_, _| {},
        )
        .unwrap();
        assert!(erased);
        assert_eq!(written, expect);
        let frames = log.borrow().frames.clone();
        let erase = frames
            .iter()
            .position(|f| f[2] == 0x03 && f[7] == 0x10)
            .unwrap();
        let bulk = frames
            .iter()
            .position(|f| f[2] == OP_SFC_FLASH_PROG_06)
            .unwrap();
        assert!(erase < bulk);
        assert_eq!(frames[bulk][0x0C], expect[0]);
    }

    #[test]
    fn a_stuck_sector_still_advances_the_cursor() {
        let (mut sfc, log) = harness(vec![cfi_buf(0x18, false), id_buf(0)]);
        let mut flash = Flash::new(&mut sfc, Mapper::Homebrew);
        flash.identify().unwrap();
        log.borrow_mut().fill = 0;
        let bus_before = log.borrow().bus.len();
        assert_eq!(flash.erase_step(0x2_0000).unwrap(), EraseStep::Pending);
        assert_eq!(log.borrow().bus.len() - bus_before, 0xC352);
        let frames = log.borrow().frames.len();
        assert_eq!(flash.erase_step(0x2_0000).unwrap(), EraseStep::Done);
        assert_eq!(log.borrow().frames.len(), frames);
    }

    #[test]
    fn narrow_non_special_id_uses_opcode_06() {
        let (mut sfc, log) = harness(vec![
            vec![0; CFI_LEN],
            cfi_buf(0x16, false),
            id_buf(0x1111_1111),
        ]);
        let mut flash = Flash::new(&mut sfc, Mapper::LoRom);
        flash.identify().unwrap();
        let before = log.borrow().frames.len();
        assert_eq!(flash.erase_step(0x100).unwrap(), EraseStep::Pending);
        let cmds = byte_cmds(&log.borrow().frames[before..]);
        assert!(cmds.iter().all(|c| c.0 == 0x05));
        assert_eq!(cmds.last().copied(), Some((0x05, 0x8AAA, 0x10)));
        log.borrow_mut().frames.clear();
        flash.program_chunk(0, &[0x44]).unwrap();
        let frames = log.borrow().frames.clone();
        assert_eq!(frames[0][2], OP_SFC_FLASH_PROG_06);
        assert_eq!(
            u16::from_le_bytes(frames[0][8..10].try_into().unwrap()),
            0x1F2
        );

        let (mut sfc, log) = harness(vec![cfi_buf(0x16, false), id_buf(0)]);
        let mut flash = Flash::new(&mut sfc, Mapper::Homebrew);
        flash.identify().unwrap();
        log.borrow_mut().frames.clear();
        flash.program_chunk(0, &[0x55]).unwrap();
        let frames = log.borrow().frames.clone();
        assert_eq!(frames[1][2], OP_SFC_FLASH_PROG_06);
    }

    #[test]
    fn every_mapper_uses_its_flash_window() {
        let cases = [
            (Mapper::CX4, 0x8000usize, 0x8000u32, 0x8000usize),
            (Mapper::ExHiRom, 0x10000, 0xC0_0000, 0x10000),
            (Mapper::SPC7110, 0x10000, 0xC0_0000, 0x10000),
            (Mapper::SDD1, 0x10000, 0xC0_0000, 0x10000),
        ];
        for (mapper, chunk, addr, total) in cases {
            let (mut sfc, log) = harness(vec![cfi_buf(0x16, false), id_buf(ID_0101)]);
            let mut flash = Flash::new(&mut sfc, mapper);
            assert_eq!(flash.chunk_size(), chunk);
            flash.identify().unwrap();
            log.borrow_mut().frames.clear();
            flash.program_chunk(0, &[0x11]).unwrap();
            let frames = log.borrow().frames.clone();
            assert!(byte_cmds(&frames).iter().all(|c| c.1 != 0x70009D));
            assert_eq!(
                u32::from_le_bytes(frames[0][4..8].try_into().unwrap()),
                addr
            );
            let mut sum = 0usize;
            for f in &frames {
                sum += usize::from(u16::from_le_bytes(f[8..10].try_into().unwrap()));
            }
            assert_eq!(sum, total);
        }
    }

    #[test]
    fn a_large_hirom_chip_erases() {
        let (mut sfc, log) = harness(vec![cfi_buf(0x18, false), id_buf(0)]);
        let mut flash = Flash::new(&mut sfc, Mapper::HiRom);
        assert_eq!(flash.identify().unwrap(), 0x100_0000);
        let before = log.borrow().frames.len();
        assert_eq!(flash.erase_step(0x1000).unwrap(), EraseStep::Pending);
        let cmds = byte_cmds(&log.borrow().frames[before..]);
        assert_eq!(cmds.last().copied(), Some((0x0B, 0x8AAA, 0x10)));
        assert!(cmds.iter().all(|c| c.1 != 0xC0_0000));
    }
}
