// SPDX-License-Identifier: GPL-2.0-only
//! Linux error numbers, as the system call dispatcher returns them.
//!
//! A system call reports a failure by returning the negated number in RAX.
//! The values are the x86-64 Linux ones, so a standard library program that
//! compares against `errno` sees the numbers it expects.

/// Operation not permitted.
pub const EPERM: i64 = -1;
/// No such file or directory.
pub const ENOENT: i64 = -2;
/// Bad file descriptor.
pub const EBADF: i64 = -9;
/// Resource temporarily unavailable.
pub const EAGAIN: i64 = -11;
/// Cannot allocate memory.
pub const ENOMEM: i64 = -12;
/// Bad address.
pub const EFAULT: i64 = -14;
/// Invalid argument.
pub const EINVAL: i64 = -22;
/// Function not implemented.
pub const ENOSYS: i64 = -38;
/// Permission denied.
///
/// Part of the Linux number table: a kernel without a filesystem or a mode bit
/// never produces it, but the dispatcher must be able to name it.
#[allow(dead_code)]
pub const EACCES: i64 = -13;
/// File exists.
pub const EEXIST: i64 = -17;
/// Not a directory.
pub const ENOTDIR: i64 = -20;
/// Is a directory.
///
/// Part of the Linux number table; nothing in this kernel opens a directory.
#[allow(dead_code)]
pub const EISDIR: i64 = -21;
/// Illegal seek.
///
/// Part of the Linux number table; this kernel implements no seek.
#[allow(dead_code)]
pub const ESPIPE: i64 = -29;
/// Numerical result out of range.
pub const ERANGE: i64 = -34;
/// Inappropriate ioctl for device.
pub const ENOTTY: i64 = -25;
/// No such device.
pub const ENODEV: i64 = -19;
/// Too many open files.
pub const EMFILE: i64 = -24;
/// No such process.
pub const ESRCH: i64 = -3;
/// Timed out.
pub const ETIMEDOUT: i64 = -110;
