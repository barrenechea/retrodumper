//! Linear ROM dump shared by every system that reads `DUMP.ROM`.
//!
//! The client walks `off` from 0 to the ROM size in `chunk_size` steps
//! (`0x4585c0`): `prepare`, then a bus read at `address(off)`. `finish`
//! runs only on the last chunk, after that read and before the write, so
//! a mapper reset still happens if the final write fails. SFC uses
//! `finish` for the SPC7110 tail reset.

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
}
