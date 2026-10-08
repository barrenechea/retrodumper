//! Retro Dumper 512-byte command frames.
//!
//! A command file (CMDn.CMD) is a sequence of 512-byte frames, each carrying
//! its own CRC-16/XMODEM. The firmware batches frames this way; a single
//! command file write may contain several frames.
//!
//! Frame layout:
//!
//! ```text
//! [0x000]      0x5A 'Z' magic (0x5B for the encrypted variant)
//! [0x001]      group
//! [0x002]      opcode
//! [0x003..0x1FD] arguments, little-endian
//! [0x1FE..0x1FF] CRC-16/XMODEM (poly 0x1021, init 0) over bytes 0x000..0x1FD,
//!                   stored big-endian (high byte @0x1FE, low byte @0x1FF)
//! ```

use crate::Error;

pub const FRAME_SIZE: usize = 512;
const CRC16_XMODEM: crc::Crc<u16> = crc::Crc::<u16>::new(&crc::CRC_16_XMODEM);
const CRC_INPUT_LEN: usize = 0x1FE;
const CRC_OFFSET: usize = 0x1FE;

pub const MAGIC: u8 = 0x5A;
/// SNOW-2.0-xored variant. The system that needs it sets this magic itself.
pub const MAGIC_ENCR: u8 = 0x5B;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    data: [u8; FRAME_SIZE],
}

impl Frame {
    pub fn new(group: u8, op: u8) -> Self {
        let mut data = [0u8; FRAME_SIZE];
        data[0] = MAGIC;
        data[1] = group;
        data[2] = op;
        Self { data }
    }

    pub fn as_bytes(&self) -> &[u8; FRAME_SIZE] {
        &self.data
    }

    pub fn finish(&mut self) {
        let crc = CRC16_XMODEM.checksum(&self.data[..CRC_INPUT_LEN]);
        self.data[CRC_OFFSET] = (crc >> 8) as u8;
        self.data[CRC_OFFSET + 1] = crc as u8;
    }

    pub fn set_u16(&mut self, off: usize, v: u16) {
        self.data[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    pub fn set_u24(&mut self, off: usize, v: u32) {
        let b = v.to_le_bytes();
        self.data[off..off + 3].copy_from_slice(&b[..3]);
    }

    pub fn set_u32(&mut self, off: usize, v: u32) {
        self.data[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    pub fn set_bytes(&mut self, off: usize, data: &[u8]) {
        assert!(
            off + data.len() <= FRAME_SIZE,
            "protocol: set_bytes overflows frame at {off} len {}",
            data.len()
        );
        self.data[off..off + data.len()].copy_from_slice(data);
    }

    pub fn get(&self, off: usize) -> u8 {
        self.data[off]
    }

    pub fn set(&mut self, off: usize, v: u8) {
        self.data[off] = v;
    }

    pub fn verify_crc(&self) -> Result<(), Error> {
        if self.data[0] != MAGIC && self.data[0] != MAGIC_ENCR {
            return Err(Error::BadMagic);
        }
        let want = u16::from_be_bytes([self.data[CRC_OFFSET], self.data[CRC_OFFSET + 1]]);
        let got = CRC16_XMODEM.checksum(&self.data[..CRC_INPUT_LEN]);
        if got != want {
            return Err(Error::CrcMismatch { got, want });
        }
        Ok(())
    }
}
