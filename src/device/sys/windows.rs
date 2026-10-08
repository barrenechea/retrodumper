//! Type names match the Win32 headers (`HANDLE`, `DWORD`, …).

#![allow(clippy::upper_case_acronyms)]

use std::io;
use std::os::raw::c_void;
use std::os::windows::io::{AsRawHandle, HandleOrInvalid, IntoRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

type HANDLE = *mut c_void;
type BOOL = i32;
type DWORD = u32;

const GENERIC_READ: DWORD = 0x8000_0000;
const GENERIC_WRITE: DWORD = 0x4000_0000;
const OPEN_EXISTING: DWORD = 3;
const CREATE_ALWAYS: DWORD = 2;
const FILE_FLAG_NO_BUFFERING: DWORD = 0x2000_0000;
const DRIVE_REMOVABLE: DWORD = 2;
const FILE_SEEK_BEGIN: DWORD = 0;
const MEM_COMMIT: DWORD = 0x1000;
const MEM_RESERVE: DWORD = 0x2000;
const MEM_RELEASE: DWORD = 0x8000;
const PAGE_READWRITE: DWORD = 0x0004;

#[repr(C)]
struct SecurityAttributes {
    n_length: DWORD,
    lp_security_descriptor: *mut c_void,
    b_inherit_handle: BOOL,
}

unsafe extern "system" {
    fn CreateFileW(
        path: *const u16,
        dw_desired_access: DWORD,
        dw_share_mode: DWORD,
        lp_security_attributes: *const SecurityAttributes,
        dw_creation_disposition: DWORD,
        dw_flags_and_attributes: DWORD,
        h_template_file: HANDLE,
    ) -> HANDLE;
    fn ReadFile(
        h_file: HANDLE,
        lp_buffer: *mut c_void,
        n_number_of_bytes_to_read: DWORD,
        lp_number_of_bytes_read: *mut DWORD,
        lp_overlapped: *mut c_void,
    ) -> BOOL;
    fn WriteFile(
        h_file: HANDLE,
        lp_buffer: *const c_void,
        n_number_of_bytes_to_write: DWORD,
        lp_number_of_bytes_written: *mut DWORD,
        lp_overlapped: *mut c_void,
    ) -> BOOL;
    fn SetFilePointerEx(
        h_file: HANDLE,
        l_distance_to_move: i64,
        new_file_pointer: *mut i64,
        dw_move_method: DWORD,
    ) -> BOOL;
    fn FlushFileBuffers(h_file: HANDLE) -> BOOL;
    fn CloseHandle(h_object: HANDLE) -> BOOL;
    fn GetDriveTypeW(lpsz_root_path_name: *const u16) -> DWORD;
    fn VirtualAlloc(
        lp_address: *mut c_void,
        dw_size: usize,
        fl_allocation_type: DWORD,
        fl_protect: DWORD,
    ) -> *mut c_void;
    fn VirtualFree(lp_address: *mut c_void, dw_size: usize, dw_free_type: DWORD) -> BOOL;
}

/// A 64 KB-aligned (hence sector-aligned) buffer via VirtualAlloc.
/// FILE_FLAG_NO_BUFFERING requires the I/O buffer, offset, and length to be
/// aligned to the device sector size; a plain Rust heap allocation is not
/// guaranteed to be, so we allocate with VirtualAlloc, which returns
/// 64 KB-aligned memory.
struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

impl AlignedBuf {
    fn new(n: usize) -> io::Result<Self> {
        // SAFETY: a null address with MEM_COMMIT|MEM_RESERVE asks the kernel
        // for a fresh region of `n` bytes. The pointer is not used unless it
        // is non-null.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                n,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        let Some(ptr) = NonNull::new(p.cast::<u8>()) else {
            return Err(io::Error::last_os_error());
        };
        Ok(Self { ptr, len: n })
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is a live VirtualAlloc region of `len` bytes, and
        // this reference does not outlive `self`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: `ptr` is the base of a VirtualAlloc region we own.
        // MEM_RELEASE (not MEM_RESERVE) frees the whole reservation; the
        // size must be 0.
        unsafe {
            VirtualFree(self.ptr.as_ptr().cast::<c_void>(), 0, MEM_RELEASE);
        }
    }
}

pub struct FileHandle {
    h: Option<OwnedHandle>,
}

impl FileHandle {
    fn raw(&self) -> HANDLE {
        self.h
            .as_ref()
            .expect("file handle used after close")
            .as_raw_handle()
    }

    fn set_pointer(&self, off: i64) -> io::Result<()> {
        let mut cur: i64 = 0;
        // SAFETY: `raw()` is an open handle we own, and `cur` is a valid
        // out-parameter.
        let r = unsafe { SetFilePointerEx(self.raw(), off, &mut cur, FILE_SEEK_BEGIN) };
        if r == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.set_pointer(off as i64)?;
        let abuf = AlignedBuf::new(buf.len())?;
        let mut read: DWORD = 0;
        // SAFETY: `abuf` is a writable allocation of `buf.len()` bytes for
        // this call, and `raw()` is an open handle we own.
        let r = unsafe {
            ReadFile(
                self.raw(),
                abuf.ptr.as_ptr().cast::<c_void>(),
                buf.len() as DWORD,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if r == 0 {
            return Err(io::Error::last_os_error());
        }
        let n = read as usize;
        buf[..n].copy_from_slice(&abuf.as_slice()[..n]);
        Ok(n)
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let abuf = AlignedBuf::new(buf.len())?;
        // SAFETY: `abuf` is a writable allocation of `buf.len()` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(buf.as_ptr(), abuf.ptr.as_ptr(), buf.len());
        }
        let mut written: DWORD = 0;
        // SAFETY: `abuf` holds `buf`'s bytes and `raw()` is an open handle.
        let r = unsafe {
            WriteFile(
                self.raw(),
                abuf.ptr.as_ptr().cast::<c_void>(),
                buf.len() as DWORD,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if r == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(written as usize)
    }

    pub fn sync(&self) -> io::Result<()> {
        // SAFETY: `raw()` is an open handle we own.
        let r = unsafe { FlushFileBuffers(self.raw()) };
        if r == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn close(mut self) -> io::Result<()> {
        let Some(owned) = self.h.take() else {
            return Ok(());
        };
        let raw = owned.into_raw_handle();
        // SAFETY: `raw` is an open handle we own and do not use again.
        // `OwnedHandle` no longer closes it.
        let r = unsafe { CloseHandle(raw) };
        if r == 0 {
            return Err(io::Error::other("retrodump: CloseHandle failed"));
        }
        Ok(())
    }
}

fn to_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn create_file(path: &Path, access: DWORD, mode: DWORD, flags: DWORD) -> io::Result<FileHandle> {
    let wide = to_wide(path);
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call.
    // CreateFileW returns either an owned handle or INVALID_HANDLE_VALUE.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            0,
            std::ptr::null(),
            mode,
            flags,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: `raw` is the CreateFileW result above and has not been closed.
    let owned = OwnedHandle::try_from(unsafe { HandleOrInvalid::from_raw_handle(raw) })
        .map_err(|_| io::Error::last_os_error())?;
    Ok(FileHandle { h: Some(owned) })
}

pub fn open_read(path: &Path) -> io::Result<FileHandle> {
    create_file(path, GENERIC_READ, OPEN_EXISTING, FILE_FLAG_NO_BUFFERING)
}

pub fn open_cmd(path: &Path) -> io::Result<FileHandle> {
    create_file(path, GENERIC_WRITE, CREATE_ALWAYS, FILE_FLAG_NO_BUFFERING)
}

/// No-op: FILE_FLAG_NO_BUFFERING reads always go to the device, even when
/// another process has the range cached.
pub fn drop_cached(_f: &FileHandle, _off: usize, _n: usize) -> io::Result<()> {
    Ok(())
}

pub fn find_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for c in b'A'..=b'Z' {
        let wide = [c as u16, b':' as u16, b'\\' as u16, 0];
        // SAFETY: `wide` is a NUL-terminated UTF-16 drive root.
        let r = unsafe { GetDriveTypeW(wide.as_ptr()) };
        if r == DRIVE_REMOVABLE {
            roots.push(PathBuf::from(format!("{}:\\", c as char)));
        }
    }
    roots
}
