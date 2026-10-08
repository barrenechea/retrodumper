//! retrodump: cross-platform SNES/SFC dumper for the Retro Base.
//!
//! The device is a USB mass-storage (FAT) drive; all communication is plain
//! file I/O.

pub mod cart;
pub mod device;
mod error;
pub mod protocol;
pub mod snes;

pub use error::{Error, Result, Stage};
