#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

pub fn fill_random(buf: &mut [u8]) -> std::io::Result<()> {
    getrandom::fill(buf).map_err(std::io::Error::from)
}
