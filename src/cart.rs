//! Linear ROM dump shared by every system that reads `DUMP.ROM`.
//!
//! The client walks `off` from 0 to the ROM size in `chunk_size` steps
//! (`0x4585c0`): `prepare`, then a bus read at `address(off)`. `finish`
//! runs only on the last chunk, after that read and before the write, so
//! a mapper reset still happens if the final write fails. SFC uses
//! `finish` for the SPC7110 tail reset.
//!
//! Flash programming (`0x458ad0`) is [`Program`]: identify the chip, erase
//! until it reports ready, then write one window at a time.

use std::borrow::Cow;
use std::io::Write;

use crate::{Error, Result};

/// Bank switch and window for one linear dump.
///
/// `read` sits on the map because `prepare` already borrows the bus.
pub trait DumpMap {
    fn chunk_size(&self) -> usize;
    fn rom_size(&self) -> usize;
    fn prepare(&mut self, off: usize) -> Result<()>;
    fn address(&self, off: usize) -> usize;
    fn read(&mut self, addr: usize, n: usize) -> Result<Vec<u8>>;
    fn finish(&mut self) -> Result<()>;
}

pub fn dump_rom(
    map: &mut impl DumpMap,
    w: &mut dyn Write,
    mut progress: impl FnMut(usize, usize),
) -> Result<()> {
    let rom_size = map.rom_size();
    if rom_size == 0 {
        return Err(Error::InvalidRomSize(rom_size));
    }
    let chunk = map.chunk_size();
    if chunk == 0 {
        return Err(Error::InvalidRomSize(0));
    }
    let mut off = 0;
    while off < rom_size {
        map.prepare(off)?;
        let n = (rom_size - off).min(chunk);
        let addr = map.address(off);
        let b = map.read(addr, n)?;
        if b.len() != n {
            return Err(Error::ShortRead {
                addr,
                got: b.len(),
                want: n,
            });
        }
        if off + n == rom_size {
            map.finish()?;
        }
        w.write_all(&b)?;
        progress(off + n, rom_size);
        off += n;
    }
    Ok(())
}

/// One erase step from the flash dispatcher (`0x472f70` with a null buffer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EraseStep {
    /// Chip-erase status is not `0xFF` yet. The client sleeps 1s, then retries.
    Wait,
    /// The erase command was issued, or the sector cursor advanced.
    /// No status sleep. A failed sector poll still returns this.
    Pending,
    /// The chip reported erased.
    Done,
}

/// Cartridge flash programming. SFC implements this; a later system with a
/// flash path implements it the same way.
pub trait Program {
    /// Bytes the dialog writes per call. `0` is "Device init error."
    fn chunk_size(&self) -> usize;
    /// CFI/ID. `Ok(0)` is "Check flash cartridge error."
    fn identify(&mut self) -> Result<usize>;
    /// Bytes `0x459200` adds before comparing the file with the chip.
    fn prefix_len(&self) -> usize {
        0
    }
    /// Bytes actually erased and programmed. Homebrew builds the menu image.
    fn image_to_write<'a>(&mut self, file: &'a [u8]) -> Cow<'a, [u8]> {
        Cow::Borrowed(file)
    }
    fn erase_step(&mut self, image_len: usize) -> Result<EraseStep>;
    fn program_chunk(&mut self, off: usize, data: &[u8]) -> Result<()>;
}

/// How many erase steps `0x458ad0` will attempt before it gives up.
fn erase_budget(flash_size: usize, image_len: usize) -> usize {
    let n = if flash_size > 0xFF_FFFF {
        if image_len <= flash_size >> 1 {
            image_len >> 16
        } else {
            flash_size >> 16
        }
    } else {
        flash_size >> 15
    };
    n + 8
}

/// Identify, reject a file `0x459200` would reject, erase, then program.
///
/// `on_size` runs after a non-zero identify and before that check. `on_erase`
/// runs only once the file has been accepted. `on_wait` is the client's 1s
/// sleep. The returned bytes are what was erased and programmed.
pub fn program_image(
    prog: &mut impl Program,
    image: &[u8],
    mut on_size: impl FnMut(usize),
    mut on_erase: impl FnMut(),
    mut on_wait: impl FnMut(),
    mut progress: impl FnMut(usize, usize),
) -> Result<Vec<u8>> {
    let chunk = prog.chunk_size();
    if chunk == 0 {
        return Err(Error::FlashInit);
    }
    let flash_size = prog.identify()?;
    if flash_size == 0 {
        return Err(Error::FlashIdentify);
    }
    on_size(flash_size);
    // Empty, or the file (plus the Homebrew menu) does not fit.
    let need = image.len().saturating_add(prog.prefix_len());
    if image.is_empty() || need > flash_size {
        return Err(Error::FlashFileSize);
    }
    let image = prog.image_to_write(image);
    debug_assert_eq!(image.len(), need);
    on_erase();
    let budget = erase_budget(flash_size, image.len());
    let mut tries = 0;
    loop {
        if tries >= budget {
            return Err(Error::FlashEraseTimeout);
        }
        match prog.erase_step(image.len())? {
            EraseStep::Done => break,
            EraseStep::Wait => {
                tries += 1;
                on_wait();
            }
            EraseStep::Pending => tries += 1,
        }
    }
    let mut off = 0;
    while off < image.len() {
        let n = chunk.min(image.len() - off);
        prog.program_chunk(off, &image[off..off + n])?;
        off += chunk;
        progress(off.min(image.len()), image.len());
    }
    Ok(image.into_owned())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Ev {
        Prepare(usize),
        Read(usize, usize),
        Finish,
        Write(usize),
    }

    struct Map {
        log: Rc<RefCell<Vec<Ev>>>,
        size: usize,
        chunk: usize,
        short: bool,
    }

    impl DumpMap for Map {
        fn chunk_size(&self) -> usize {
            self.chunk
        }
        fn rom_size(&self) -> usize {
            self.size
        }
        fn prepare(&mut self, off: usize) -> Result<()> {
            self.log.borrow_mut().push(Ev::Prepare(off));
            Ok(())
        }
        fn address(&self, off: usize) -> usize {
            off + 0x80
        }
        fn read(&mut self, addr: usize, n: usize) -> Result<Vec<u8>> {
            self.log.borrow_mut().push(Ev::Read(addr, n));
            if self.short {
                Ok(vec![0; n / 2])
            } else {
                Ok(vec![0xA5; n])
            }
        }
        fn finish(&mut self) -> Result<()> {
            self.log.borrow_mut().push(Ev::Finish);
            Ok(())
        }
    }

    struct LogWrite {
        log: Rc<RefCell<Vec<Ev>>>,
        buf: Vec<u8>,
    }

    impl Write for LogWrite {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.log.borrow_mut().push(Ev::Write(buf.len()));
            self.buf.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn run(size: usize, chunk: usize, short: bool) -> (Vec<Ev>, Vec<u8>, Result<()>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut map = Map {
            log: Rc::clone(&log),
            size,
            chunk,
            short,
        };
        let mut w = LogWrite {
            log: Rc::clone(&log),
            buf: Vec::new(),
        };
        let result = dump_rom(&mut map, &mut w, |_, _| {});
        (log.borrow().clone(), w.buf, result)
    }

    #[test]
    fn finish_runs_before_the_last_write() {
        let (ev, buf, result) = run(0x30, 0x10, false);
        assert!(result.is_ok());
        assert_eq!(buf, vec![0xA5; 0x30]);
        assert_eq!(
            ev,
            vec![
                Ev::Prepare(0x00),
                Ev::Read(0x80, 0x10),
                Ev::Write(0x10),
                Ev::Prepare(0x10),
                Ev::Read(0x90, 0x10),
                Ev::Write(0x10),
                Ev::Prepare(0x20),
                Ev::Read(0xA0, 0x10),
                Ev::Finish,
                Ev::Write(0x10),
            ]
        );
    }

    #[test]
    fn last_chunk_shorter_than_the_window_still_finishes() {
        let (ev, buf, result) = run(0x18, 0x10, false);
        assert!(result.is_ok());
        assert_eq!(buf.len(), 0x18);
        assert_eq!(
            ev,
            vec![
                Ev::Prepare(0x00),
                Ev::Read(0x80, 0x10),
                Ev::Write(0x10),
                Ev::Prepare(0x10),
                Ev::Read(0x90, 0x08),
                Ev::Finish,
                Ev::Write(0x08),
            ]
        );
    }

    #[test]
    fn short_read_stops_before_finish_and_write() {
        let (ev, buf, result) = run(0x20, 0x10, true);
        assert!(matches!(
            result,
            Err(Error::ShortRead {
                addr: 0x80,
                got: 0x08,
                want: 0x10
            })
        ));
        assert!(buf.is_empty());
        assert_eq!(ev, vec![Ev::Prepare(0), Ev::Read(0x80, 0x10)]);
    }

    #[test]
    fn short_read_on_the_final_chunk_skips_finish() {
        let (ev, buf, result) = run(0x10, 0x10, true);
        assert!(matches!(
            result,
            Err(Error::ShortRead {
                addr: 0x80,
                got: 0x08,
                want: 0x10
            })
        ));
        assert!(buf.is_empty());
        assert_eq!(ev, vec![Ev::Prepare(0), Ev::Read(0x80, 0x10)]);
    }

    #[test]
    fn zero_rom_or_chunk_is_rejected() {
        let (ev, _, result) = run(0, 0x10, false);
        assert!(matches!(result, Err(Error::InvalidRomSize(0))));
        assert!(ev.is_empty());

        let (ev, _, result) = run(0x20, 0, false);
        assert!(matches!(result, Err(Error::InvalidRomSize(0))));
        assert!(ev.is_empty());
    }

    struct Stub {
        size: usize,
        chunk: usize,
        steps: Vec<EraseStep>,
        at: usize,
        erase_lens: Vec<usize>,
        programmed: Vec<(usize, Vec<u8>)>,
        identified: bool,
    }

    impl Program for Stub {
        fn chunk_size(&self) -> usize {
            self.chunk
        }
        fn identify(&mut self) -> Result<usize> {
            self.identified = true;
            Ok(self.size)
        }
        fn erase_step(&mut self, image_len: usize) -> Result<EraseStep> {
            self.erase_lens.push(image_len);
            let step = self.steps.get(self.at).copied().unwrap_or(EraseStep::Wait);
            self.at += 1;
            Ok(step)
        }
        fn program_chunk(&mut self, off: usize, data: &[u8]) -> Result<()> {
            self.programmed.push((off, data.to_vec()));
            Ok(())
        }
    }

    #[test]
    fn erase_budget_matches_the_dialog() {
        // 4 MB is not above 0xFFFFFF, so the file length is not consulted.
        assert_eq!(erase_budget(0x40_0000, 0x20_0000), (0x40_0000 >> 15) + 8);
        assert_eq!(erase_budget(0x40_0000, 0x30_0000), (0x40_0000 >> 15) + 8);
        assert_eq!(erase_budget(0xFF_FFFF, 1), (0xFF_FFFF >> 15) + 8);
        assert_eq!(erase_budget(0x8_0000, 0x8_0000), (0x8_0000 >> 15) + 8);
        assert_eq!(erase_budget(0x100_0000, 1), 8);
        assert_eq!(erase_budget(0x100_0000, 0x80_0000), 0x80 + 8);
        assert_eq!(erase_budget(0x100_0000, 0x80_0001), 0x100 + 8);
    }

    #[test]
    fn program_image_erases_then_writes_windows() {
        let mut prog = Stub {
            size: 100,
            chunk: 16,
            steps: vec![EraseStep::Pending, EraseStep::Wait, EraseStep::Done],
            at: 0,
            erase_lens: vec![],
            programmed: vec![],
            identified: false,
        };
        let mut waits = 0;
        let mut seen = 0;
        let image = vec![0x11; 20];
        program_image(
            &mut prog,
            &image,
            |n| seen = n,
            || {},
            || waits += 1,
            |_, _| {},
        )
        .unwrap();
        assert!(prog.identified);
        assert_eq!(seen, 100);
        assert_eq!(waits, 1);
        assert_eq!(prog.erase_lens, vec![20, 20, 20]);
        assert_eq!(
            prog.programmed,
            vec![(0, vec![0x11; 16]), (16, vec![0x11; 4])]
        );
    }

    #[test]
    fn program_image_stops_when_erase_never_finishes() {
        let mut prog = Stub {
            size: 1,
            chunk: 16,
            steps: vec![],
            at: 0,
            erase_lens: vec![],
            programmed: vec![],
            identified: false,
        };
        let mut waits = 0;
        let err =
            program_image(&mut prog, &[1], |_| {}, || {}, || waits += 1, |_, _| {}).unwrap_err();
        assert_eq!(err.to_string(), "snes: Erase flash cartridge error.");
        assert_eq!(waits, 8);
        assert!(prog.programmed.is_empty());
    }

    #[test]
    fn pending_erase_steps_do_not_sleep() {
        let mut prog = Stub {
            size: 1,
            chunk: 16,
            steps: vec![EraseStep::Pending; 8],
            at: 0,
            erase_lens: vec![],
            programmed: vec![],
            identified: false,
        };
        let mut waits = 0;
        let err =
            program_image(&mut prog, &[1], |_| {}, || {}, || waits += 1, |_, _| {}).unwrap_err();
        assert!(matches!(err, Error::FlashEraseTimeout));
        assert_eq!(waits, 0);
        assert_eq!(prog.erase_lens.len(), 8);
        assert!(prog.programmed.is_empty());
    }

    #[test]
    fn program_image_rejects_before_erase() {
        let mut prog = Stub {
            size: 4,
            chunk: 16,
            steps: vec![EraseStep::Done],
            at: 0,
            erase_lens: vec![],
            programmed: vec![],
            identified: false,
        };
        let mut saw_size = false;
        let mut erased = false;
        let err = program_image(
            &mut prog,
            &[],
            |_| saw_size = true,
            || erased = true,
            || {},
            |_, _| {},
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "snes: ROM file size error.");
        assert!(prog.identified);
        assert!(saw_size);
        assert!(!erased);
        assert!(prog.erase_lens.is_empty());

        saw_size = false;
        let err = program_image(
            &mut prog,
            &[1, 2, 3, 4, 5],
            |_| saw_size = true,
            || erased = true,
            || {},
            |_, _| {},
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "snes: ROM file size error.");
        assert!(saw_size);
        assert!(!erased);
        assert!(prog.erase_lens.is_empty());

        prog.chunk = 0;
        saw_size = false;
        let err = program_image(
            &mut prog,
            &[1],
            |_| saw_size = true,
            || {},
            || {},
            |_, _| {},
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "snes: Device init error.");
        assert!(!saw_size);

        prog.chunk = 16;
        prog.size = 0;
        let err = program_image(
            &mut prog,
            &[1],
            |_| saw_size = true,
            || {},
            || {},
            |_, _| {},
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "snes: Check flash cartridge error.");
        assert!(!saw_size);
        assert!(prog.erase_lens.is_empty());
        assert_eq!(Error::FlashVerifyFailed.to_string(), "snes: Check failure.");
    }
}
