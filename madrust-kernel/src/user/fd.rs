// SPDX-License-Identifier: GPL-2.0-only
//! User file descriptor table.
//!
//! This kernel has no filesystem, so the descriptors a program can hold are
//! the three standard streams, duplicated handles to them, and anonymous
//! pipes. The table keeps those, which is enough for the standard library
//! startup path: `stdout` is descriptor one, `dup` and `dup2` work, `fstat`
//! reports the descriptor type, and `fcntl` reports the status flags.
//!
//! A pipe is a fixed 4096-byte buffer. A read from an empty pipe and a write
//! to a full one return `EAGAIN` instead of blocking, because no program
//! reached through the acceptance boot waits on one; blocking pipe waits would
//! need a second wait queue next to the futex one.
//!
//! The table is protected by the interrupt flag, like the futex wait queue:
//! every operation is a few array writes and no operation blocks.

use core::cell::UnsafeCell;

use crate::console::console;
use crate::sync;

use super::errno::{EAGAIN, EBADF, EINVAL, EMFILE, ENOTTY, EPERM};

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

/// `O_WRONLY`: a write only stream.
const O_WRONLY: u64 = 1;

/// What a descriptor refers to.
#[derive(Clone, Copy)]
enum Kind {
    /// One of the three standard streams; writes reach the serial console.
    Console,
    /// Read end of a pipe.
    PipeRead(usize),
    /// Write end of a pipe.
    PipeWrite(usize),
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
        descriptors[0] = Some(Kind::Console);
        descriptors[1] = Some(Kind::Console);
        descriptors[2] = Some(Kind::Console);
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

/// `write`: send bytes to the console or to the write end of a pipe.
pub fn write(descriptor: u64, bytes: &[u8]) -> i64 {
    let guard = lock();
    let Some(kind) = descriptor_kind(&guard.table.descriptors, descriptor) else {
        return EBADF;
    };
    match kind {
        Kind::Console => {
            console().write_bytes(bytes);
            i64::try_from(bytes.len()).unwrap_or(i64::MAX)
        }
        Kind::PipeRead(_) => EBADF,
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

/// `read`: take bytes from the read end of a pipe.
///
/// The standard input is an empty console, so a read from it reports end of
/// file.
pub fn read(descriptor: u64, buffer: &mut [u8]) -> i64 {
    let guard = lock();
    let Some(kind) = descriptor_kind(&guard.table.descriptors, descriptor) else {
        return EBADF;
    };
    match kind {
        Kind::Console => 0,
        Kind::PipeWrite(_) => EBADF,
        Kind::PipeRead(index) => {
            let pipe = &mut guard.table.pipes[index];
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
    }
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
                Some(Kind::Console) => 0,
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
    let mode = match kind {
        Kind::Console => S_IFCHR | 0o666,
        Kind::PipeRead(_) | Kind::PipeWrite(_) => S_IFIFO | 0o600,
    };
    Ok(stat_block(mode, 1))
}

/// Build a zeroed `struct stat` with the fields a program reads.
fn stat_block(mode: u32, links: u64) -> [u8; STAT_SIZE] {
    let mut buffer = [0u8; STAT_SIZE];
    buffer[8..16].copy_from_slice(&1u64.to_le_bytes()); // st_ino
    buffer[16..24].copy_from_slice(&links.to_le_bytes()); // st_nlink
    buffer[24..28].copy_from_slice(&mode.to_le_bytes()); // st_mode
    buffer[56..64].copy_from_slice(&4096i64.to_le_bytes()); // st_blksize
    buffer
}

/// `true` when a read of `descriptor` can return data now.
pub fn readable(descriptor: u64) -> bool {
    let guard = lock();
    match descriptor_kind(&guard.table.descriptors, descriptor) {
        Some(Kind::Console) => true,
        Some(Kind::PipeRead(index)) => guard.table.pipes[index].length > 0,
        Some(Kind::PipeWrite(_)) => false,
        None => false,
    }
}

/// `true` when a write of `descriptor` can make progress now.
pub fn writable(descriptor: u64) -> bool {
    let guard = lock();
    match descriptor_kind(&guard.table.descriptors, descriptor) {
        Some(Kind::Console) => true,
        Some(Kind::PipeWrite(index)) => guard.table.pipes[index].length < PIPE_CAPACITY,
        Some(Kind::PipeRead(_)) => false,
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
