//! Opens go through [`std::fs::OpenOptions`] so the access mode, `O_CREAT`,
//! and the trailing NUL on the path come from std. Cache-bypass flags, `mmap`,
//! and `O_DIRECT` come from `libc`: macOS `O_CREAT` is `0x200` and `F_NOCACHE`
//! is 48, and Linux `O_DIRECT` differs between x86_64 (`0x4000`) and aarch64
//! (`0x10000`).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
#[cfg(not(target_os = "linux"))]
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub struct FileHandle {
    file: Option<File>,
}

impl FileHandle {
    fn file(&self) -> &File {
        self.file.as_ref().expect("file handle used after close")
    }

    fn raw(&self) -> RawFd {
        self.file().as_raw_fd()
    }

    pub fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        #[cfg(target_os = "linux")]
        {
            read_aligned(self.raw(), buf, off)
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.file().read_at(buf, off)
        }
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        #[cfg(target_os = "linux")]
        {
            write_aligned(self.raw(), buf)
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.file().write_at(buf, 0)
        }
    }

    pub fn sync(&self) -> io::Result<()> {
        self.file().sync_all()
    }

    pub fn close(mut self) -> io::Result<()> {
        let Some(file) = self.file.take() else {
            return Ok(());
        };
        let raw = file.into_raw_fd();
        // SAFETY: `raw` is an open fd we own and do not use again. `File` no
        // longer closes it.
        if unsafe { libc::close(raw) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn nocache(fd: RawFd, cmd: libc::c_int) -> io::Result<()> {
    // SAFETY: `fd` is open and `cmd` is a macOS fcntl that takes an int.
    if unsafe { libc::fcntl(fd, cmd, 1) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `O_DIRECT` (Linux) requires the I/O buffer, offset, and length to be
/// aligned to the device's logical sector, so read through an anonymous
/// page-aligned (hence 512-aligned) mmap buffer and copy to the caller's
/// (unaligned) slice.
#[cfg(target_os = "linux")]
fn read_aligned(fd: RawFd, buf: &mut [u8], off: u64) -> io::Result<usize> {
    let mapped = Mapped::anon(buf.len())?;
    // SAFETY: `mapped` is a writable anonymous mapping of `buf.len()` bytes
    // for this call, and `fd` is open.
    let n = unsafe { libc::pread(fd, mapped.ptr, buf.len(), off as libc::off_t) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let n = n as usize;
    // SAFETY: `pread` wrote `n` bytes at `mapped.ptr` on success.
    buf[..n].copy_from_slice(unsafe { std::slice::from_raw_parts(mapped.ptr.cast::<u8>(), n) });
    Ok(n)
}

#[cfg(target_os = "linux")]
fn write_aligned(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    let mapped = Mapped::anon(buf.len())?;
    // SAFETY: `mapped` is a writable anonymous mapping of `buf.len()` bytes.
    unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), mapped.ptr.cast::<u8>(), buf.len()) };
    // SAFETY: the mapping holds `buf`'s bytes and `fd` is open.
    let n = unsafe { libc::pwrite(fd, mapped.ptr, buf.len(), 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

struct Mapped {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Mapped {
    #[cfg(target_os = "linux")]
    fn anon(len: usize) -> io::Result<Self> {
        Self::map(
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    }

    fn map(
        len: usize,
        prot: libc::c_int,
        flags: libc::c_int,
        fd: RawFd,
        offset: libc::off_t,
    ) -> io::Result<Self> {
        // SAFETY: `len` is the mapping length. `fd` is -1 for anonymous maps
        // and an open descriptor for file maps. `offset` is page-aligned
        // when `fd` is a file.
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, fd, offset) };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { ptr, len })
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` came from a successful `mmap` and have not been
        // unmapped yet.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

pub fn open_read(path: &Path) -> io::Result<FileHandle> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(target_os = "linux")]
    opts.custom_flags(libc::O_DIRECT);
    let file = opts.open(path)?;
    #[cfg(target_os = "macos")]
    nocache(file.as_raw_fd(), libc::F_GLOBAL_NOCACHE)?;
    Ok(FileHandle { file: Some(file) })
}

/// Open the command file for an in-place write at offset 0. It must not
/// truncate: the host FAT driver frees the file's cluster on truncate and
/// fails with EIO ("cluster N is free where it should be in use") once it has
/// re-read the firmware's FAT, which does not keep the allocation.
pub fn open_cmd(path: &Path) -> io::Result<FileHandle> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).mode(0o644);
    #[cfg(target_os = "linux")]
    opts.custom_flags(libc::O_DIRECT);
    let file = opts.open(path)?;
    #[cfg(target_os = "macos")]
    {
        // F_NOCACHE on a data fd is the closest equivalent of
        // FILE_FLAG_NO_BUFFERING: the frame goes straight to the device.
        nocache(file.as_raw_fd(), libc::F_NOCACHE)?;
    }
    Ok(FileHandle { file: Some(file) })
}

/// The host page size. `mmap` offsets have to be aligned to it, which is
/// 16 KiB on Apple Silicon.
#[cfg(target_os = "macos")]
fn page_size() -> io::Result<usize> {
    // SAFETY: `sysconf(_SC_PAGESIZE)` only reads a process constant.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(n).map_err(|_| io::Error::other("invalid page size"))
}

/// Evict any pages of `[off, off+n)` resident in the unified buffer cache
/// (macOS). F_NOCACHE/F_GLOBAL_NOCACHE only stop our reads from populating the
/// cache; a read is still served from pages another process (Spotlight, Quick
/// Look, a plain cp) already cached, and for the live T-DRIVER.DMP / DUMP.ROM
/// windows those are stale. msync(MS_INVALIDATE) on a shared mapping of the
/// range drops them; mapping reads nothing.
pub fn drop_cached(f: &FileHandle, off: usize, n: usize) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        if n == 0 {
            return Ok(());
        }
        let pg = page_size()?;
        if pg == 0 || !pg.is_power_of_two() {
            return Err(io::Error::other("invalid page size"));
        }
        let start = off & !(pg - 1);
        let len = (off + n) - start;
        let mapped = Mapped::map(
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            f.raw(),
            start as libc::off_t,
        )?;
        // SAFETY: `mapped` covers `len` bytes of the shared file mapping.
        let r = unsafe { libc::msync(mapped.ptr, len, libc::MS_INVALIDATE) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (f, off, n);
        Ok(())
    }
}

pub fn find_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(target_os = "macos")]
    {
        if let Ok(entries) = std::fs::read_dir("/Volumes") {
            for e in entries.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy().into_owned();
                match name.as_str() {
                    "Macintosh HD" | "Recovery" | "Preboot" | "VM" | "No volume name" => {}
                    _ => roots.push(PathBuf::from("/Volumes").join(name)),
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        // Mounted FAT/exfat volumes.
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            for line in mounts.lines() {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() >= 3 {
                    match fields[2] {
                        "vfat" | "fat" | "msdos" | "exfat" | "ntfs" => {
                            roots.push(PathBuf::from(fields[1]));
                        }
                        _ => {}
                    }
                }
            }
        }
        // Fallback: common media directories.
        for base in ["/media", "/mnt"] {
            if let Ok(entries) = std::fs::read_dir(base) {
                for e in entries.flatten() {
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        roots.push(PathBuf::from(base).join(e.file_name()));
                    }
                }
            }
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_match_host_access_modes_and_do_not_truncate() {
        let path = std::env::temp_dir().join(format!(
            "retrodump-open-flags-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(&path, [0xABu8; 8]).unwrap();
        let _rm = Rm(&path);

        let cmd = open_cmd(&path).unwrap();
        // SAFETY: the fd is open for the duration of the fcntl.
        let cmd_flags = unsafe { libc::fcntl(cmd.raw(), libc::F_GETFL) };
        assert!(cmd_flags >= 0);
        assert_eq!(cmd_flags & libc::O_ACCMODE, libc::O_WRONLY);
        #[cfg(target_os = "linux")]
        assert_ne!(cmd_flags & libc::O_DIRECT, 0);
        cmd.close().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 8);

        let rd = open_read(&path).unwrap();
        // SAFETY: the fd is open for the duration of the fcntl.
        let rd_flags = unsafe { libc::fcntl(rd.raw(), libc::F_GETFL) };
        assert!(rd_flags >= 0);
        assert_eq!(rd_flags & libc::O_ACCMODE, libc::O_RDONLY);
        rd.close().unwrap();
    }

    struct Rm<'a>(&'a Path);
    impl Drop for Rm<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
}
