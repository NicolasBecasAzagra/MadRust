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
/// Is a directory.
pub const EISDIR: i64 = -21;
/// Illegal seek.
pub const ESPIPE: i64 = -29;
/// Read-only file system.
pub const EROFS: i64 = -30;
/// Broken pipe.
pub const EPIPE: i64 = -32;
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
/// Socket operation on a non-socket.
pub const ENOTSOCK: i64 = -88;
/// Protocol not supported.
pub const EPROTONOSUPPORT: i64 = -93;
/// Operation not supported.
pub const EOPNOTSUPP: i64 = -95;
/// Address family not supported by the protocol.
pub const EAFNOSUPPORT: i64 = -97;
/// Address already in use.
pub const EADDRINUSE: i64 = -98;
/// Cannot assign the requested address.
pub const EADDRNOTAVAIL: i64 = -99;
/// Network is unreachable.
pub const ENETUNREACH: i64 = -101;
/// Connection reset by peer.
pub const ECONNRESET: i64 = -104;
/// Transport endpoint is already connected.
pub const EISCONN: i64 = -106;
/// Transport endpoint is not connected.
pub const ENOTCONN: i64 = -107;
/// Timed out.
pub const ETIMEDOUT: i64 = -110;
/// Connection refused.
pub const ECONNREFUSED: i64 = -111;
/// No route to host.
pub const EHOSTUNREACH: i64 = -113;
