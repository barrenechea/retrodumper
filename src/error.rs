use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// First underlying error from the volume scan.
    NotFound(String),
    /// The volume has no valid T-DRIVER info block.
    NoDevice,
    /// Still failing after the retries were exhausted.
    Unresponsive {
        what: String,
        source: Box<Error>,
    },
    Transport {
        stage: Stage,
        source: Box<Error>,
    },
    BadFrameSize {
        got: usize,
        want: usize,
    },
    BadMagic,
    CrcMismatch {
        got: u16,
        want: u16,
    },
    /// Header block is all 0xFF: no cart is inserted.
    NoCartridge,
    NoSram,
    /// Save read-back did not match what was written.
    VerifyFailed,
    ShortRead {
        addr: usize,
        got: usize,
        want: usize,
    },
    InvalidRomSize(usize),
    /// `addr + n` runs past the 24-bit `DUMP.ROM` window (`0x1000000`).
    BusOutOfRange {
        addr: usize,
        n: usize,
    },
    /// Shown as-is. `Display` does not add a prefix.
    Cli(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The device NAKed the open.
    Open,
    /// EIO or a short transfer on a file that did open.
    Io,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Missing file. `retry` returns these immediately.
    pub fn is_not_found(&self) -> bool {
        match self {
            Error::Io(e) => e.kind() == std::io::ErrorKind::NotFound,
            Error::Transport { source, .. } => source.is_not_found(),
            _ => false,
        }
    }

    pub fn stage(&self) -> Option<Stage> {
        match self {
            Error::Transport { stage, .. } => Some(*stage),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "retrodump: {e}"),
            Error::NotFound(d) => write!(f, "retrodump: no Retro Dumper device found: {d}"),
            Error::NoDevice => write!(f, "retrodump: no valid T-DRIVER info block"),
            Error::Unresponsive { what, source } => {
                write!(
                    f,
                    "retrodump: device not responding: {what}: {source} (replug the device)"
                )
            }
            Error::Transport { source, .. } => write!(f, "{source}"),
            Error::BadFrameSize { got, want } => write!(f, "retrodump: frame size {got} != {want}"),
            Error::BadMagic => write!(f, "protocol: bad magic"),
            Error::CrcMismatch { got, want } => {
                write!(
                    f,
                    "protocol: crc mismatch: got 0x{got:04X} want 0x{want:04X}"
                )
            }
            Error::NoCartridge => {
                write!(f, "snes: no cartridge detected (header block is all 0xFF)")
            }
            Error::NoSram => write!(f, "snes: mapper has no SRAM mapping"),
            Error::VerifyFailed => write!(f, "snes: save read-back verification failed"),
            Error::ShortRead { addr, got, want } => {
                write!(
                    f,
                    "retrodump: short bus read at 0x{addr:X}: got {got} want {want}"
                )
            }
            Error::InvalidRomSize(s) => write!(f, "retrodump: invalid rom size {s}"),
            Error::BusOutOfRange { addr, n } => {
                write!(f, "retrodump: bus read out of range: 0x{addr:X}+{n}")
            }
            Error::Cli(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
