// SPDX-License-Identifier: GPL-2.0-only
//! User file descriptor table.
//!
//! The descriptors a program can hold are the three standard streams,
//! duplicated handles to them, anonymous pipes, entries of the initramfs
//! archive, and the two character devices that archive carries. The table keeps
//! those, which is enough for the standard library startup path and for a
//! program that opens and reads a file.
//!
//! ## Files read in place
//!
//! A file descriptor holds the [`Entry`] the archive parser returned and the
//! current offset. `Entry` borrows the archive bytes from the reserved module
//! memory, so no file is copied into the kernel heap. `read` copies from that
//! borrowed slice straight into the caller's buffer; because the kernel and the
//! program share one address space, the destination is the user buffer itself.
//! The archive is read only, so an attempt to open for writing returns `EROFS`.
//!
//! A pipe is a fixed 4096-byte buffer. A read from an empty pipe and a write
//! to a full one return `EAGAIN` instead of blocking, because no program
//! reached through the acceptance boot waits on one; blocking pipe waits would
//! need a second wait queue next to the futex one.
//!
//! The table is protected by the interrupt flag, like the futex wait queue:
//! every operation is a few array writes and no operation blocks.

use core::cell::UnsafeCell;

use madrust_initramfs::Entry;
use madrust_mm::virt_to_phys;

use crate::console::console;
use crate::sync;

use super::errno::{
    EAGAIN, EBADF, EINVAL, EISDIR, EMFILE, ENODEV, ENOENT, ENOTTY, EPERM, EROFS, ESPIPE,
};

/// Number of descriptors in the table.
const MAX_DESCRIPTORS: usize = 32;

/// Number of pipes the table can create.
const MAX_PIPES: usize = 4;

/// Capacity of one pipe buffer.
const PIPE_CAPACITY: usize = 4096;

/// Size of the x86-64 `struct stat`, in bytes.
pub const STAT_SIZE: usize = 144;

/// `S_IFCHR`: character device.
const S_IFCHR: u32 = 0o020000;
/// `S_IFIFO`: named pipe.
const S_IFIFO: u32 = 0o010000;

/// `O_ACCMODE`: the access mode bits of an `openat` flags word.
const O_ACCMODE: u64 = 0o3;
/// `O_WRONLY`: a write only stream.
const O_WRONLY: u64 = 1;

/// `fcntl` command: duplicate with a minimum descriptor number.
const F_DUPFD: u64 = 0;
/// `fcntl` command: read the descriptor flags.
const F_GETFD: u64 = 1;
/// `fcntl` command: set the descriptor flags.
const F_SETFD: u64 = 2;
/// `fcntl` command: read the status flags.
const F_GETFL: u64 = 3;
/// `fcntl` command: set the status flags.
const F_SETFL: u64 = 4;
/// `fcntl` command: duplicate with a minimum number and close-on-exec.
const F_DUPFD_CLOEXEC: u64 = 1030;

/// `lseek` origin: the start of the file.
const SEEK_SET: u64 = 0;
/// `lseek` origin: the current offset.
const SEEK_CUR: u64 = 1;
/// `lseek` origin: the end of the file.
const SEEK_END: u64 = 2;

/// One open file of the initramfs.
#[derive(Clone, Copy)]
struct File {
    /// Archive entry that backs the descriptor.
    entry: Entry<'static>,
    /// Offset of the next byte `read` returns.
    offset: usize,
}

/// What a descriptor refers to.
#[derive(Clone, Copy)]
enum Kind {
    /// One of the three standard streams, or `/dev/console`; writes reach the
    /// serial console. The stored mode is what `fstat` reports.
    Console(u32),
    /// `/dev/null`: writes are discarded and reads report end of file. The
    /// stored mode is what `fstat` reports.
    Null(u32),
    /// Read end of a pipe.
    PipeRead(usize),
    /// Write end of a pipe.
    PipeWrite(usize),
    /// One file of the initramfs archive.
    File(File),
}

/// One anonymous pipe.
struct Pipe {
    buffer: [u8; PIPE_CAPACITY],
    read: usize,
    length: usize,
}

impl Pipe {
    const EMPTY: Self = Self {
        buffer: [0; PIPE_CAPACITY],
        read: 0,
        length: 0,
    };
}

struct Table {
    descriptors: [Option<Kind>; MAX_DESCRIPTORS],
    pipes: [Pipe; MAX_PIPES],
}

impl Table {
    /// A table with the three standard streams open.
    const fn new() -> Self {
        let mut descriptors = [None; MAX_DESCRIPTORS];
        descriptors[0] = Some(Kind::Console(S_IFCHR | 0o666));
        descriptors[1] = Some(Kind::Console(S_IFCHR | 0o666));
        descriptors[2] = Some(Kind::Console(S_IFCHR | 0o666));
        Self {
            descriptors,
            pipes: [Pipe::EMPTY, Pipe::EMPTY, Pipe::EMPTY, Pipe::EMPTY],
        }
    }

    /// First free descriptor at or above `minimum`.
    fn free_from(&self, minimum: usize) -> Option<usize> {
        (minimum..MAX_DESCRIPTORS).find(|&index| self.descriptors[index].is_none())
    }
}

struct Cell(UnsafeCell<Table>);

// SAFETY: the table is only reached through `lock`, which holds the interrupt
// flag for the duration of one operation.
unsafe impl Sync for Cell {}

static TABLE: Cell = Cell(UnsafeCell::new(Table::new()));

/// Short critical section over the descriptor table.
struct Guard<'a> {
    table: &'a mut Table,
    flags: u64,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        sync::interrupt_restore(self.flags);
    }
}

/// Enter the critical section.
fn lock() -> Guard<'static> {
    let flags = sync::interrupt_save_disable();
    // SAFETY: interrupts are disabled, so no other path can borrow the table
    // until the guard is dropped.
    let table = unsafe { &mut *TABLE.0.get() };
    Guard { table, flags }
}

/// `openat`: open a read only file of the initramfs.
///
/// The access mode must be `O_RDONLY`, because the archive cannot be written.
/// A directory reports `EISDIR`, a missing entry reports `ENOENT`, and a path
/// that climbs above the archive root is rejected the same way. The two
/// character devices the archive carries keep their console and null
/// behaviour; any other device node reports `ENODEV`.
pub fn open(path: &[u8], flags: u64) -> i64 {
    if flags & O_ACCMODE != 0 {
        return EROFS;
    }
    let entry = match super::initramfs::resolve(path) {
        Ok(Some(entry)) => entry,
        Ok(None) | Err(_) => return ENOENT,
    };
    let kind = if entry.is_regular_file() {
        Kind::File(File { entry, offset: 0 })
    } else if entry.is_character_device() {
        match entry.normalized_name() {
            Some("dev/console") => Kind::Console(entry.mode()),
            Some("dev/null") => Kind::Null(entry.mode()),
            _ => return ENODEV,
        }
    } else if entry.is_directory() {
        return EISDIR;
    } else {
        return EINVAL;
    };

    let guard = lock();
    let Some(index) = guard.table.free_from(0) else {
        return EMFILE;
    };
    guard.table.descriptors[index] = Some(kind);
    i64::try_from(index).unwrap_or(i64::MAX)
}

/// `write`: send bytes to the console, discard them for `/dev/null`, or fill the
/// write end of a pipe.
pub fn write(descriptor: u64, bytes: &[u8]) -> i64 {
    let guard = lock();
    let Some(kind) = descriptor_kind(&guard.table.descriptors, descriptor) else {
        return EBADF;
    };
    match kind {
        Kind::Console(_) => {
            console().write_bytes(bytes);
            i64::try_from(bytes.len()).unwrap_or(i64::MAX)
        }
        Kind::Null(_) => i64::try_from(bytes.len()).unwrap_or(i64::MAX),
        Kind::PipeRead(_) | Kind::File(_) => EBADF,
        Kind::PipeWrite(index) => {
            if bytes.is_empty() {
                return 0;
            }
            let pipe = &mut guard.table.pipes[index];
            let room = PIPE_CAPACITY - pipe.length;
            let count = room.min(bytes.len());
            if count == 0 {
                return EAGAIN;
            }
            let mut written = 0;
            while written < count {
                // `pipe.length` already counts the bytes copied so far, so the
                // write position is its sum with the read position.
                let position = (pipe.read + pipe.length) % PIPE_CAPACITY;
                let chunk = (PIPE_CAPACITY - position).min(count - written);
                pipe.buffer[position..position + chunk]
                    .copy_from_slice(&bytes[written..written + chunk]);
                pipe.length += chunk;
                written += chunk;
            }
            i64::try_from(count).unwrap_or(i64::MAX)
        }
    }
}

/// `read`: take bytes from a file, a pipe or the console.
///
/// A file read copies from the archive entry at the current offset and advances
/// it; reading at or past the end reports zero. The standard input is an empty
/// console and `/dev/null` has no data, so both report end of file.
pub fn read(descriptor: u64, buffer: &mut [u8]) -> i64 {
    let guard = lock();
    let Ok(index) = usize::try_from(descriptor) else {
        return EBADF;
    };
    let Some(kind) = guard.table.descriptors.get(index).copied().flatten() else {
        return EBADF;
    };
    match kind {
        Kind::Console(_) | Kind::Null(_) => 0,
        Kind::PipeWrite(_) => EBADF,
        Kind::PipeRead(pipe_index) => {
            let pipe = &mut guard.table.pipes[pipe_index];
            if pipe.length == 0 {
                return EAGAIN;
            }
            let count = pipe.length.min(buffer.len());
            let mut taken = 0;
            while taken < count {
                let position = (pipe.read + taken) % PIPE_CAPACITY;
                let chunk = (PIPE_CAPACITY - position).min(count - taken);
                buffer[taken..taken + chunk]
                    .copy_from_slice(&pipe.buffer[position..position + chunk]);
                taken += chunk;
            }
            pipe.read = (pipe.read + count) % PIPE_CAPACITY;
            pipe.length -= count;
            i64::try_from(count).unwrap_or(i64::MAX)
        }
        Kind::File(mut file) => {
            let remaining = file.entry.data().get(file.offset..).unwrap_or(&[]);
            let count = remaining.len().min(buffer.len());
            buffer[..count].copy_from_slice(&remaining[..count]);
            file.offset += count;
            guard.table.descriptors[index] = Some(Kind::File(file));
            i64::try_from(count).unwrap_or(i64::MAX)
        }
    }
}

/// `lseek`: move the offset of a file descriptor.
///
/// Only archive entries can seek; the console, `/dev/null` and the pipes report
/// `ESPIPE`. The new offset is relative to the start, the current offset or the
/// end of the file, and a negative result is rejected with `EINVAL`. Seeking
/// past the end is allowed, like Linux, and the next read reports zero.
pub fn seek(descriptor: u64, offset: i64, whence: u64) -> i64 {
    let guard = lock();
    let Ok(index) = usize::try_from(descriptor) else {
        return EBADF;
    };
    let Some(kind) = guard.table.descriptors.get(index).copied().flatten() else {
        return EBADF;
    };
    let Kind::File(mut file) = kind else {
        return ESPIPE;
    };
    let base: i128 = match whence {
        SEEK_SET => 0,
        SEEK_CUR => file.offset as i128,
        SEEK_END => file.entry.data().len() as i128,
        _ => return EINVAL,
    };
    let target = base + i128::from(offset);
    if target < 0 {
        return EINVAL;
    }
    let Ok(target) = usize::try_from(target) else {
        return EINVAL;
    };
    file.offset = target;
    guard.table.descriptors[index] = Some(Kind::File(file));
    i64::try_from(target).unwrap_or(i64::MAX)
}

/// Physical address and length of the file behind a descriptor.
///
/// `mmap` uses it to point the page table entry at the archive bytes instead
/// of copying them. The entry data comes from reserved module memory, so the
/// address is stable and through the direct map. A descriptor that is not a
/// regular file reports `ENODEV`.
pub fn file_backing(descriptor: u64) -> Result<(u64, u64), i64> {
    let guard = lock();
    let Some(kind) = descriptor_kind(&guard.table.descriptors, descriptor) else {
        return Err(EBADF);
    };
    let Kind::File(file) = kind else {
        return Err(ENODEV);
    };
    let data = file.entry.data();
    let Some(physical) = virt_to_phys(data.as_ptr() as u64) else {
        return Err(ENODEV);
    };
    Ok((physical, data.len() as u64))
}

/// `close`: release one descriptor.
pub fn close(descriptor: u64) -> i64 {
    let guard = lock();
    let Ok(index) = usize::try_from(descriptor) else {
        return EBADF;
    };
    let Some(slot) = guard.table.descriptors.get_mut(index) else {
        return EBADF;
    };
    if slot.is_none() {
        return EBADF;
    }
    *slot = None;
    0
}

/// `dup`: duplicate a descriptor at the lowest free number.
pub fn dup(descriptor: u64, minimum: u64) -> i64 {
    let Ok(minimum) = usize::try_from(minimum) else {
        return EINVAL;
    };
    if minimum >= MAX_DESCRIPTORS {
        return EMFILE;
    }
    duplicate(descriptor, None, minimum)
}

/// `dup2` and `dup3`: duplicate a descriptor at an exact number.
pub fn dup_to(descriptor: u64, target: u64) -> i64 {
    let Ok(target) = usize::try_from(target) else {
        return EBADF;
    };
    if target >= MAX_DESCRIPTORS {
        return EBADF;
    }
    duplicate(descriptor, Some(target), target)
}

/// Shared implementation of the three duplication calls.
fn duplicate(descriptor: u64, exact: Option<usize>, minimum: usize) -> i64 {
    let guard = lock();
    let Some(kind) = descriptor_kind(&guard.table.descriptors, descriptor) else {
        return EBADF;
    };
    let index = match exact {
        Some(index) => {
            if usize::try_from(descriptor).ok() == Some(index) {
                return i64::try_from(index).unwrap_or(i64::MAX);
            }
            index
        }
        None => match guard.table.free_from(minimum) {
            Some(index) => index,
            None => return EMFILE,
        },
    };
    guard.table.descriptors[index] = Some(kind);
    i64::try_from(index).unwrap_or(i64::MAX)
}

/// `pipe2`: create a pipe and store the two descriptors at `address`.
///
/// Returns the descriptor numbers packed the way the caller expects: the
/// function returns zero and writes two `int` values into the user array.
pub fn create_pipe() -> Result<(u64, u64), i64> {
    let guard = lock();
    let Some(pipe) = (0..MAX_PIPES).find(|&index| {
        !guard
            .table
            .descriptors
            .iter()
            .flatten()
            .any(|kind| matches!(kind, Kind::PipeRead(i) | Kind::PipeWrite(i) if *i == index))
    }) else {
        return Err(EMFILE);
    };
    let Some(read) = guard.table.free_from(0) else {
        return Err(EMFILE);
    };
    guard.table.descriptors[read] = Some(Kind::PipeRead(pipe));
    let Some(write) = guard.table.free_from(0) else {
        guard.table.descriptors[read] = None;
        return Err(EMFILE);
    };
    guard.table.descriptors[write] = Some(Kind::PipeWrite(pipe));
    guard.table.pipes[pipe] = Pipe::EMPTY;
    Ok((
        u64::try_from(read).unwrap_or(0),
        u64::try_from(write).unwrap_or(0),
    ))
}

/// `fcntl`: descriptor and status flags, duplication, and nothing else.
pub fn fcntl(descriptor: u64, command: u64, argument: u64) -> i64 {
    match command {
        F_DUPFD => dup(descriptor, argument),
        F_DUPFD_CLOEXEC => dup(descriptor, argument),
        F_GETFD | F_SETFD | F_SETFL => {
            let guard = lock();
            if descriptor_kind(&guard.table.descriptors, descriptor).is_some() {
                0
            } else {
                EBADF
            }
        }
        F_GETFL => {
            let guard = lock();
            match descriptor_kind(&guard.table.descriptors, descriptor) {
                Some(Kind::Console(_) | Kind::Null(_) | Kind::File(_)) => 0,
                Some(Kind::PipeWrite(_)) => O_WRONLY as i64,
                Some(Kind::PipeRead(_)) => 0,
                None => EBADF,
            }
        }
        _ => EINVAL,
    }
}

/// `fstat`: describe one descriptor.
pub fn stat(descriptor: u64) -> Result<[u8; STAT_SIZE], i64> {
    let guard = lock();
    let Some(kind) = descriptor_kind(&guard.table.descriptors, descriptor) else {
        return Err(EBADF);
    };
    let (mode, size) = match kind {
        Kind::Console(mode) | Kind::Null(mode) => (mode, 0),
        Kind::PipeRead(_) | Kind::PipeWrite(_) => (S_IFIFO | 0o600, 0),
        Kind::File(file) => (file.entry.mode(), file.entry.data().len() as u64),
    };
    Ok(stat_block(mode, 1, size))
}

/// `newfstatat`: describe one path of the archive.
///
/// The entry mode comes from the archive, so a program that was stored with an
/// execute bit still reports it. A path that climbs above the root or names no
/// entry reports `ENOENT`, exactly like `openat`.
pub fn stat_path(path: &[u8]) -> Result<[u8; STAT_SIZE], i64> {
    match super::initramfs::resolve(path) {
        Ok(Some(entry)) => Ok(stat_block(entry.mode(), 1, entry.data().len() as u64)),
        Ok(None) | Err(_) => Err(ENOENT),
    }
}

/// Build a zeroed `struct stat` with the fields a program reads.
fn stat_block(mode: u32, links: u64, size: u64) -> [u8; STAT_SIZE] {
    let mut buffer = [0u8; STAT_SIZE];
    buffer[8..16].copy_from_slice(&1u64.to_le_bytes()); // st_ino
    buffer[16..24].copy_from_slice(&links.to_le_bytes()); // st_nlink
    buffer[24..28].copy_from_slice(&mode.to_le_bytes()); // st_mode
    buffer[48..56].copy_from_slice(&size.to_le_bytes()); // st_size
    buffer[56..64].copy_from_slice(&4096i64.to_le_bytes()); // st_blksize
    buffer[64..72].copy_from_slice(&size.div_ceil(512).to_le_bytes()); // st_blocks
    buffer
}

/// `true` when a read of `descriptor` can return data now.
pub fn readable(descriptor: u64) -> bool {
    let guard = lock();
    match descriptor_kind(&guard.table.descriptors, descriptor) {
        Some(Kind::Console(_) | Kind::Null(_) | Kind::File(_)) => true,
        Some(Kind::PipeRead(index)) => guard.table.pipes[index].length > 0,
        Some(Kind::PipeWrite(_)) => false,
        None => false,
    }
}

/// `true` when a write of `descriptor` can make progress now.
pub fn writable(descriptor: u64) -> bool {
    let guard = lock();
    match descriptor_kind(&guard.table.descriptors, descriptor) {
        Some(Kind::Console(_) | Kind::Null(_)) => true,
        Some(Kind::PipeWrite(index)) => guard.table.pipes[index].length < PIPE_CAPACITY,
        Some(Kind::PipeRead(_) | Kind::File(_)) => false,
        None => false,
    }
}

/// `true` when `descriptor` names an open descriptor.
pub fn is_open(descriptor: u64) -> bool {
    let guard = lock();
    descriptor_kind(&guard.table.descriptors, descriptor).is_some()
}

/// Resolve a descriptor number, rejecting numbers outside the table.
fn descriptor_kind(descriptors: &[Option<Kind>; MAX_DESCRIPTORS], descriptor: u64) -> Option<Kind> {
    let index = usize::try_from(descriptor).ok()?;
    *descriptors.get(index)?
}

/// `ioctl`: no device control operation is supported.
pub const fn ioctl_error() -> i64 {
    ENOTTY
}

/// `prlimit64`: report `EPERM` for a change, because limits are fixed.
pub const fn limit_change_error() -> i64 {
    EPERM
}
