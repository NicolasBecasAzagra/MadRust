// SPDX-License-Identifier: GPL-2.0-only
//! A read only file mapping over the initramfs file descriptor.
//!
//! The model is opened once, mapped with `mmap(PROT_READ, MAP_PRIVATE)` and
//! marked with `madvise(MADV_HUGEPAGE)`. The mapping covers every whole page of
//! the file. The final partial page cannot be mapped on MadRust, whose file
//! mappings must end inside the file, so those last at most 4095 bytes are read
//! once into a small buffer and the accessors fall back to it.
//!
//! On a kernel with large file pages, the first access to each aligned 2 MiB
//! chunk copies the file data into an aligned block and maps that block read
//! only. The copy is the kernel's price for the alignment; the runtime still
//! makes only the one copy of the final partial page. For a model that ends
//! with a tensor, that tensor's final reads come from the buffer; the other
//! weights are read through the mapping.
//!
//! [`Mapping::pin`] asks `mlock` for the mapped range after the model is
//! mapped and before the first token. `mlock` faults every page in, so the
//! weights become resident and pinned instead of merely mapped, and the cost
//! of the guarantee lands in the first token. The final partial page is not
//! part of the mapping and is not pinned.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd};

use crate::error::{Error, Result};

/// Page size of the target.
const PAGE_SIZE: usize = 4096;

const SYS_OPENAT: u64 = 257;
const SYS_MMAP: u64 = 9;
const SYS_MUNMAP: u64 = 11;
const SYS_MADVISE: u64 = 28;
const SYS_MLOCK: u64 = 149;
const AT_FDCWD: u64 = -100i64 as u64;
const O_RDONLY: u64 = 0;
const PROT_READ: u64 = 1;
const MAP_PRIVATE: u64 = 2;
const MADV_HUGEPAGE: u64 = 14;

/// Open a path from the initramfs read only.
///
/// The standard library opens a file with a path of its own on this target, and
/// MadRust implements the Linux `openat` entry point instead. The descriptor is
/// wrapped in a [`File`], so reads, seeks and the final close use the same
/// standard library code as everywhere else.
pub fn open_readonly(path: &str) -> Result<File> {
    match open_descriptor(path) {
        Ok(descriptor) => {
            // SAFETY: the kernel returned a fresh descriptor for this process
            // and nothing else owns it.
            Ok(unsafe { File::from_raw_fd(descriptor) })
        }
        Err(error) => Err(Error::new(format!(
            "open {path} failed with error {}",
            -error
        ))),
    }
}

/// Open a path, reporting a missing file as `None`.
///
/// The config file is optional: the runtime falls back to its documented
/// defaults when it is absent. Every other error is still a failure.
pub fn open_readonly_optional(path: &str) -> Result<Option<File>> {
    match open_descriptor(path) {
        Ok(descriptor) => {
            // SAFETY: the kernel returned a fresh descriptor for this process
            // and nothing else owns it.
            Ok(Some(unsafe { File::from_raw_fd(descriptor) }))
        }
        // `ENOENT` is 2 on x86-64 Linux, the value MadRust returns too.
        Err(2) => Ok(None),
        Err(error) => Err(Error::new(format!(
            "open {path} failed with error {}",
            -error
        ))),
    }
}

/// `openat(AT_FDCWD, path, O_RDONLY)`.
///
/// Returns the raw descriptor, or the negative Linux error number.
fn open_descriptor(path: &str) -> core::result::Result<i32, i64> {
    let mut terminated = Vec::with_capacity(path.len() + 1);
    terminated.extend_from_slice(path.as_bytes());
    terminated.push(0);
    let descriptor: i64;
    // SAFETY: `terminated` is a live NUL terminated string and AT_FDCWD with a
    // read only flag is the call both kernels implement.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_OPENAT as i64 => descriptor,
            in("rdi") AT_FDCWD,
            in("rsi") terminated.as_ptr() as u64,
            in("rdx") O_RDONLY,
            in("r10") 0u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if (-4095..0).contains(&descriptor) {
        Err(-descriptor)
    } else {
        Ok(descriptor as i32)
    }
}

/// A mapped file plus its final partial page.
pub struct Mapping {
    base: *mut u8,
    mapped_len: usize,
    file_len: usize,
    huge: bool,
    tail: Vec<u8>,
}

impl Mapping {
    /// Open `path` and map its whole pages read only.
    pub fn open(path: &str) -> Result<Self> {
        let mut file = open_readonly(path)?;
        let file_len = usize::try_from(file.metadata()?.len())
            .map_err(|_| Error::new("mapping: file is too large for this target"))?;
        let mapped_len = file_len / PAGE_SIZE * PAGE_SIZE;

        let (base, huge) = if mapped_len == 0 {
            (core::ptr::null_mut(), false)
        } else {
            // SAFETY: `file` is open read only for the duration of the call and
            // the kernel validates the descriptor and the length. The flags
            // ask for a private, read only mapping of whole pages.
            let base = unsafe { map_read(file.as_raw_fd(), mapped_len)? };
            // The advice is a hint, not a requirement: a kernel that does not
            // implement file large pages reports a failure and the mapping
            // stays correct at 4 KiB.
            (base, request_huge_pages(base, mapped_len))
        };

        let mut tail = Vec::new();
        if mapped_len < file_len {
            tail.resize(file_len - mapped_len, 0);
            file.seek(SeekFrom::Start(mapped_len as u64))?;
            file.read_exact(&mut tail)?;
        }

        Ok(Self {
            base,
            mapped_len,
            file_len,
            huge,
            tail,
        })
    }

    /// Total size of the file.
    pub fn file_len(&self) -> usize {
        self.file_len
    }

    /// Bytes that are mapped, always a multiple of the page size.
    pub fn mapped_len(&self) -> usize {
        self.mapped_len
    }

    /// `true` when the kernel accepted `madvise(MADV_HUGEPAGE)` for the range.
    pub fn huge_pages_requested(&self) -> bool {
        self.huge
    }

    /// The mapped prefix.
    pub fn prefix(&self) -> &[u8] {
        if self.mapped_len == 0 {
            return &[];
        }
        // SAFETY: `base` points at `mapped_len` bytes the kernel mapped read
        // only, the mapping lives as long as `self`, and the slice is shared.
        unsafe { core::slice::from_raw_parts(self.base, self.mapped_len) }
    }

    /// The mapped bytes from `offset` to the end of the mapping.
    ///
    /// The blocked matrix kernel needs a contiguous weight range. Everything
    /// before the final partial page is contiguous in the mapping, so a caller
    /// can take these bytes and fall back to the element accessors for the few
    /// rows the tail still holds. An offset past the mapping yields an empty
    /// slice.
    pub fn bytes_from(&self, offset: usize) -> &[u8] {
        if offset >= self.mapped_len {
            return &[];
        }
        &self.prefix()[offset..]
    }

    /// Make every mapped page resident and pin it.
    ///
    /// `mlock` faults the range in and records a pin per page or per large
    /// block, so the weights are no longer merely mapped: the kernel will not
    /// reclaim them until the process exits or unlocks them. The address and
    /// the length are page aligned by construction, which is what the call
    /// requires. Returns the positive error number on failure.
    pub fn pin(&self) -> core::result::Result<(), i64> {
        if self.mapped_len == 0 {
            return Ok(());
        }
        let result: i64;
        // SAFETY: the range is the successful `mmap` result, it is page
        // aligned and non-empty, and `mlock` only marks pages the process
        // already mapped.
        unsafe {
            core::arch::asm!(
                "syscall",
                inlateout("rax") SYS_MLOCK as i64 => result,
                in("rdi") self.base as u64,
                in("rsi") self.mapped_len as u64,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
        }
        if (-4095..0).contains(&result) {
            Err(-result)
        } else {
            Ok(())
        }
    }

    /// Read a byte range into a new buffer.
    ///
    /// Used for the header, which is metadata rather than weights.
    pub fn read_bytes(&self, offset: usize, length: usize) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(length)
            .ok_or_else(|| Error::new("mapping: byte range overflow"))?;
        if end > self.file_len {
            return Err(Error::new("mapping: byte range past the end of the file"));
        }
        let mapped_end = end.min(self.mapped_len);
        let mut out = Vec::with_capacity(length);
        if offset < mapped_end {
            out.extend_from_slice(&self.prefix()[offset..mapped_end]);
        }
        if end > self.mapped_len {
            let tail_start = offset.max(self.mapped_len) - self.mapped_len;
            let tail_end = end - self.mapped_len;
            out.extend_from_slice(&self.tail[tail_start..tail_end]);
        }
        Ok(out)
    }

    /// Copy a byte range into `out`.
    ///
    /// The range may straddle the end of the mapping, which is where a block of
    /// a quantized tensor can land: the packed block is bytes, not an aligned
    /// element, so the element accessors cannot read it.
    pub fn copy_bytes(&self, offset: usize, out: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(out.len())
            .ok_or_else(|| Error::new("mapping: byte range overflow"))?;
        if end > self.file_len {
            return Err(Error::new("mapping: byte range past the end of the file"));
        }
        self.copy_from_file(offset, out);
        Ok(())
    }

    /// Little endian `u16` at a file offset.
    pub fn u16_at(&self, offset: usize) -> u16 {
        if offset + 2 <= self.mapped_len {
            // SAFETY: the range is inside the mapped prefix and the read is
            // unaligned, which is what a tensor offset allows.
            unsafe {
                u16::from_le(core::ptr::read_unaligned(
                    self.base.add(offset) as *const u16
                ))
            }
        } else {
            let mut bytes = [0u8; 2];
            self.copy_from_file(offset, &mut bytes);
            u16::from_le_bytes(bytes)
        }
    }

    /// Little endian `u32` at a file offset.
    pub fn u32_at(&self, offset: usize) -> u32 {
        if offset + 4 <= self.mapped_len {
            // SAFETY: the range is inside the mapped prefix and the read is
            // unaligned.
            unsafe {
                u32::from_le(core::ptr::read_unaligned(
                    self.base.add(offset) as *const u32
                ))
            }
        } else {
            let mut bytes = [0u8; 4];
            self.copy_from_file(offset, &mut bytes);
            u32::from_le_bytes(bytes)
        }
    }

    /// Copy a few bytes that can straddle the end of the mapping.
    fn copy_from_file(&self, offset: usize, out: &mut [u8]) {
        let end = offset + out.len();
        let prefix_end = end.min(self.mapped_len);
        let from_prefix = prefix_end.saturating_sub(offset);
        if from_prefix != 0 {
            out[..from_prefix].copy_from_slice(&self.prefix()[offset..prefix_end]);
        }
        if end > self.mapped_len {
            let tail_start = offset.max(self.mapped_len) - self.mapped_len;
            let tail_end = end - self.mapped_len;
            out[from_prefix..].copy_from_slice(&self.tail[tail_start..tail_end]);
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        if self.mapped_len == 0 {
            return;
        }
        // SAFETY: `base` and `mapped_len` are the exact values a successful
        // `mmap` returned and nothing else in the program unmapped them.
        unsafe {
            unmap(self.base, self.mapped_len);
        }
    }
}

/// Map `length` bytes of `descriptor` read only.
///
/// # Safety
///
/// The caller must keep `descriptor` open for the duration of the mapping and
/// must not touch the returned range except through the mapping.
unsafe fn map_read(descriptor: i32, length: usize) -> Result<*mut u8> {
    let address: i64;
    // SAFETY: the caller upholds the descriptor contract; the instruction only
    // performs the system call the kernel validates.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_MMAP as i64 => address,
            in("rdi") 0u64,
            in("rsi") length as u64,
            in("rdx") PROT_READ,
            in("r10") MAP_PRIVATE,
            in("r8") descriptor as u64,
            in("r9") 0u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if (-4095..0).contains(&address) {
        Err(Error::new(format!(
            "mapping: mmap failed with error {}",
            -address
        )))
    } else {
        Ok(address as *mut u8)
    }
}

/// Ask the kernel to back `[base, base + length)` with 2 MiB pages.
///
/// Returns `true` when the kernel accepted the advice. The mapping is already
/// valid without it, so a refusal only means the range stays on 4 KiB pages.
fn request_huge_pages(base: *mut u8, length: usize) -> bool {
    let result: i64;
    // SAFETY: `base` and `length` are the values a successful `mmap` returned,
    // and `madvise` only marks the range; the kernel validates the whole range.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_MADVISE as i64 => result,
            in("rdi") base as u64,
            in("rsi") length as u64,
            in("rdx") MADV_HUGEPAGE,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result == 0
}

/// Remove a mapping.
///
/// # Safety
///
/// `address` and `length` must be the values a successful `mmap` returned.
unsafe fn unmap(address: *mut u8, length: usize) {
    let result: i64;
    // SAFETY: the caller upholds the mapping contract above.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_MUNMAP as i64 => result,
            in("rdi") address as u64,
            in("rsi") length as u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    let _ = result;
}
