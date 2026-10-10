//! The USB mass-storage (MSC) transport used by the Retro Base dumper.
//!
//! The device enumerates as a removable FAT drive. Communication is plain
//! file I/O:
//!
//! - `T-DRIVER.DMP` — device info block at offset 0x2000; command responses
//!   are read from offset 0
//! - `DUMP.ROM` — window onto the cartridge bus; file offset = system bus
//!   address. The window check is 24-bit (`0x1000000`), which covers SFC
//!   and the Sega carts (SMS, Game Gear, Mega Drive).
//! - `CMD0.CMD`..`CMD3.CMD` — commands. Both MSC writers format `%sCMD%d.CMD`
//!   from byte `0x154584a`, then store `(n+1) & 3` before `CreateFileW`.
//!
//! # MSC transport robustness
//!
//! The volume is a FAT image synthesized by the firmware, but the host FAT
//! driver (macOS FSKit msdos, Linux vfat) caches metadata and trusts it. The
//! firmware does not keep the host's cluster allocations, so once the driver
//! re-reads the FAT (e.g. after the device has been idle) any operation that
//! frees a host-allocated cluster fails: truncating CMD.CMD fails "cluster N
//! is free where it should be in use" with EIO, on every open, until the
//! volume is remounted. That was the "wedge".
//!
//! Unix `open_cmd` therefore creates the file without `O_TRUNC`, which removes
//! the wedge at its source. Windows uses `CREATE_ALWAYS`, which replaces an
//! existing file. The filename rotation does not replace that. What remains
//! are transient failures, retried here with exponential backoff; a failure
//! that outlasts the backoff
//! is reported as `Error::Unresponsive`.

pub mod sys;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::protocol;
use crate::{Error, Result};

pub use sys::FileHandle;

/// What a cartridge dumper can ask of the device.
///
/// `Device` implements this. Tests inject a recording fake. SFC uses it
/// today; SMS, Game Gear, and Mega Drive will use the same four calls.
pub trait Bus {
    fn info(&self) -> &Info;
    fn send(&mut self, frames: &[&[u8]]) -> Result<()>;
    fn read_response(&mut self, n: usize) -> Result<Vec<u8>>;
    fn read_bus(&mut self, addr: usize, n: usize) -> Result<Vec<u8>>;
}

pub const INFO_FILE_NAME: &str = "T-DRIVER.DMP";
pub const DUMP_FILE_NAME: &str = "DUMP.ROM";
/// 2020 firmware put this block at 0x8000.
const INFO_BLOCK_OFF: usize = 0x2000;
/// The I/O unit: reads must stay sector-aligned and in multiples of this size.
const SECTOR: usize = 0x400;

// The first retry delay; it doubles up to BACKOFF_MAX.
const BACKOFF_START: Duration = Duration::from_millis(50);
const BACKOFF_MAX: Duration = Duration::from_secs(2);

// How many consecutive failed opens (NAKs) are tried before giving up:
// 7 attempts wait 50+100+200+400+800+1600 ms = 3.15 s in total.
const OPEN_ATTEMPTS: u32 = 7;
// How many failed or short reads/writes on an opened file are tried
// (reopening each time) before giving up.
const IO_ATTEMPTS: u32 = 3;
// Before the first I/O after this much idle time, read the T-DRIVER info
// block to wake the device.
const IDLE_WAKE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default)]
pub struct Info {
    /// e.g. "SFC_L072_FW400".
    pub name: String,
    /// Bootloader name, e.g. "BOOT_SFC_L072_V003".
    pub second: String,
    pub id0: u32,
    pub id1: u32,
    /// e.g. 20231009.
    pub version: i64,
    /// 2020 firmware layout.
    pub legacy: bool,
}

pub struct Device {
    /// Volume root, e.g. "/Volumes/SFC".
    pub root: PathBuf,
    pub info: Info,
    cmd_n: u32,
    /// End of the last successful I/O, for the idle wake.
    last_io: Option<Instant>,
}

impl Device {
    pub fn find() -> Result<Self> {
        let mut first_err: Option<Box<Error>> = None;
        for root in sys::find_roots() {
            match Self::open(root) {
                Ok(d) => return Ok(d),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(Box::new(e));
                    }
                }
            }
        }
        match first_err {
            Some(e) => Err(Error::NotFound(e.to_string())),
            None => Err(Error::NotFound("no removable volumes found".into())),
        }
    }

    pub fn open(root: PathBuf) -> Result<Self> {
        let mut buf: Option<Vec<u8>> = None;
        let res = retry("info", || {
            let b = read_raw(&root, INFO_FILE_NAME, INFO_BLOCK_OFF, SECTOR)?;
            buf = Some(b);
            Ok(())
        });
        res.map_err(|e| if e.is_not_found() { e } else { Error::NoDevice })?;
        let info = parse_info(&buf.expect("open did not run")).ok_or(Error::NoDevice)?;
        Ok(Device {
            root,
            info,
            cmd_n: 0,
            last_io: Some(Instant::now()),
        })
    }
}

impl Bus for Device {
    fn info(&self) -> &Info {
        &self.info
    }

    /// The firmware finishes the command inside this write, so the next
    /// `DUMP.ROM` read needs no settle delay.
    fn send(&mut self, frames: &[&[u8]]) -> Result<()> {
        let mut all = Vec::new();
        for f in frames {
            if f.len() != protocol::FRAME_SIZE {
                return Err(Error::BadFrameSize {
                    got: f.len(),
                    want: protocol::FRAME_SIZE,
                });
            }
            all.extend_from_slice(f);
        }
        let n = self.cmd_n;
        self.cmd_n = (self.cmd_n + 1) & 3;
        let name = format!("CMD{n}.CMD");
        let root = self.root.clone();
        let what = format!("send {name}");
        self.io(&what, move || {
            let path = root.join(&name);
            let f = sys::open_cmd(&path).map_err(|e| Error::Transport {
                stage: crate::Stage::Open,
                source: Box::new(Error::Io(e)),
            })?;
            let w = match f.write(&all) {
                Ok(w) => w,
                Err(e) => {
                    let _ = f.close();
                    return Err(Error::Transport {
                        stage: crate::Stage::Io,
                        source: Box::new(Error::Io(e)),
                    });
                }
            };
            if w != all.len() {
                let _ = f.close();
                return Err(Error::Transport {
                    stage: crate::Stage::Io,
                    source: Box::new(Error::Io(std::io::Error::other("short write"))),
                });
            }
            f.close().map_err(|e| Error::Transport {
                stage: crate::Stage::Io,
                source: Box::new(Error::Io(e)),
            })
        })
    }

    fn read_response(&mut self, n: usize) -> Result<Vec<u8>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        self.read(INFO_FILE_NAME, 0, n)
    }

    fn read_bus(&mut self, addr: usize, n: usize) -> Result<Vec<u8>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        if addr + n > 0x100_0000 {
            return Err(Error::BusOutOfRange { addr, n });
        }
        self.read(DUMP_FILE_NAME, addr, n)
    }
}

impl Device {
    fn read(&mut self, name: &str, off: usize, n: usize) -> Result<Vec<u8>> {
        let root = self.root.clone();
        let name = name.to_string();
        let what = format!("read {name}");
        let mut out: Option<Vec<u8>> = None;
        self.io(&what, || {
            let b = read_raw(&root, &name, off, n)?;
            out = Some(b);
            Ok(())
        })?;
        Ok(out.expect("read did not run"))
    }

    fn io(&mut self, what: &str, f: impl FnMut() -> Result<()>) -> Result<()> {
        if let Some(last) = self.last_io
            && last.elapsed() > IDLE_WAKE
        {
            let root = self.root.clone();
            retry("wake", || {
                let _b = read_raw(&root, INFO_FILE_NAME, INFO_BLOCK_OFF, SECTOR)?;
                Ok(())
            })?;
        }
        retry(what, f)?;
        self.last_io = Some(Instant::now());
        Ok(())
    }
}

pub fn parse_info(b: &[u8]) -> Option<Info> {
    if b.len() < SECTOR {
        return None;
    }
    let mut info = Info::default();
    if u32::from_le_bytes([b[0], b[1], b[2], b[3]]) == 0xA55A_AA55 {
        // New format.
        info.id0 = u32::from_le_bytes([b[0x08], b[0x09], b[0x0A], b[0x0B]]);
        info.id1 = u32::from_le_bytes([b[0x0C], b[0x0D], b[0x0E], b[0x0F]]);
        info.name = cstr(&b[0x10..0x30]);
        info.second = cstr(&b[0x30..0x50]);
        info.version = atoi(&cstr(&b[0x50..0x400]));
    } else if (b[0] as char).is_ascii_uppercase() {
        // Legacy (2020 firmware) format.
        info.legacy = true;
        info.name = cstr(&b[0x00..0x100]);
        info.second = cstr(&b[0x100..0x200]);
        info.id0 = u32::from_le_bytes([b[0x200], b[0x201], b[0x202], b[0x203]]);
        info.id1 = u32::from_le_bytes([b[0x204], b[0x205], b[0x206], b[0x207]]);
        info.version = atoi(&cstr(&b[0x3F8..0x400]));
    } else {
        return None;
    }
    Some(info)
}

fn read_raw(root: &std::path::Path, name: &str, off: usize, n: usize) -> Result<Vec<u8>> {
    let path = root.join(name);
    let start = off & !(SECTOR - 1);
    let end = (off + n + SECTOR - 1) & !(SECTOR - 1);
    let f = sys::open_read(&path).map_err(|e| Error::Transport {
        stage: crate::Stage::Open,
        source: Box::new(Error::Io(e)),
    })?;
    if let Err(e) = sys::drop_cached(&f, start, end - start) {
        let _ = f.close();
        return Err(Error::Transport {
            stage: crate::Stage::Io,
            source: Box::new(Error::Io(e)),
        });
    }
    let mut buf = vec![0u8; end - start];
    let mut fail: Option<Error> = None;
    for i in (0..buf.len()).step_by(SECTOR) {
        let chunk = &mut buf[i..i + SECTOR];
        match f.read_at(chunk, (start + i) as u64) {
            Ok(m) if m < SECTOR => {
                fail = Some(Error::Transport {
                    stage: crate::Stage::Io,
                    source: Box::new(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "short read",
                    ))),
                });
                break;
            }
            Ok(_) => {}
            Err(e) => {
                fail = Some(Error::Transport {
                    stage: crate::Stage::Io,
                    source: Box::new(Error::Io(e)),
                });
                break;
            }
        }
    }
    let _ = f.close();
    if let Some(e) = fail {
        return Err(e);
    }
    Ok(buf[off - start..off - start + n].to_vec())
}

/// Run `f` until it succeeds. Failed opens are retried up to
/// `OPEN_ATTEMPTS` times and failed transfers up to `IO_ATTEMPTS` times,
/// with an exponential backoff shared by both. A missing file is not
/// retried.
fn retry(what: &str, mut f: impl FnMut() -> Result<()>) -> Result<()> {
    let (mut opens, mut ios) = (0u32, 0u32);
    let mut delay = BACKOFF_START;
    loop {
        match f() {
            Ok(()) => return Ok(()),
            Err(e) if e.is_not_found() => return Err(e),
            Err(e) => {
                match e.stage() {
                    Some(crate::Stage::Io) => ios += 1,
                    _ => opens += 1,
                }
                if opens >= OPEN_ATTEMPTS || ios >= IO_ATTEMPTS {
                    return Err(Error::Unresponsive {
                        what: what.to_string(),
                        source: Box::new(e),
                    });
                }
                std::thread::sleep(delay);
                delay = (delay * 2).min(BACKOFF_MAX);
            }
        }
    }
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy_owned(b[..end].to_vec())
}

fn atoi(s: &str) -> i64 {
    s.trim().parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_info_new_format() {
        let mut b = [0u8; SECTOR];
        b[0..4].copy_from_slice(&0xA55A_AA55u32.to_le_bytes());
        b[0x08..0x0C].copy_from_slice(&1234u32.to_le_bytes());
        b[0x0C..0x10].copy_from_slice(&5678u32.to_le_bytes());
        b[0x10..0x10 + 16].copy_from_slice(b"GEN_PCB02_FW400\0");
        b[0x50..0x50 + 8].copy_from_slice(b"20231009");
        let info = parse_info(&b).unwrap();
        assert_eq!(info.name, "GEN_PCB02_FW400");
        assert_eq!(info.id0, 1234);
        assert_eq!(info.id1, 5678);
        assert_eq!(info.version, 20231009);
        assert!(!info.legacy);
    }

    #[test]
    fn parse_info_legacy_format() {
        let mut b = [0u8; SECTOR];
        b[0..10].copy_from_slice(b"PCB02TESTX"); // starts with an uppercase letter
        b[0x100..0x100 + 4].copy_from_slice(b"BOOT");
        b[0x200..0x204].copy_from_slice(&99u32.to_le_bytes());
        b[0x204..0x208].copy_from_slice(&100u32.to_le_bytes());
        b[0x3F8..0x3F8 + 3].copy_from_slice(b"400");
        let info = parse_info(&b).unwrap();
        assert!(info.legacy);
        assert_eq!(info.name, "PCB02TESTX");
        assert_eq!(info.second, "BOOT");
        assert_eq!(info.id0, 99);
        assert_eq!(info.id1, 100);
        assert_eq!(info.version, 400);
    }

    #[test]
    fn parse_info_rejects_garbage() {
        let b = [0x00u8; SECTOR];
        assert!(parse_info(&b).is_none());
        let b = [0x61u8; SECTOR]; // lowercase 'a' is not a legacy marker
        assert!(parse_info(&b).is_none());
    }
}
