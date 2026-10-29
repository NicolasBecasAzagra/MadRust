// SPDX-License-Identifier: GPL-2.0-only
//! The protocols of the path from the guest to the host.
//!
//! Five protocols carry one datagram out of the machine: an Ethernet frame,
//! an address resolution request that finds the hardware address of the
//! gateway, an IPv4 header, a UDP header and a TCP connection. Each module of
//! this crate owns one of them, and each one is plain data: addresses are
//! arrays of bytes, ports and lengths are the numbers of the wire format, and
//! every field is written where the specification puts it.
//!
//! Nothing here touches hardware or allocates memory. Every parser borrows the
//! bytes it reads, checks the length before the first read and returns the
//! reason of a refusal, so the same code runs in the kernel and in host unit
//! tests. The checksum of IPv4, UDP and TCP share the one's complement sum of
//! [`sum`].
//!
//! The order of the modules is the order of the path: [`ethernet`] carries an
//! [`arp`] packet while no hardware address is known, then [`ipv4`] carries a
//! [`udp`] datagram or a [`tcp`] connection once it is.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

use core::fmt;

pub mod arp;
pub mod ethernet;
pub mod ipv4;
pub mod tcp;
pub mod udp;

/// One's complement sum of the bytes of every part.
///
/// The bytes are read as big endian 16-bit words. An odd total length is
/// completed with a zero byte, which is what the Internet checksum of RFC 1071
/// does. The parts are summed as if they were concatenated, so a caller may
/// pass the fields of a header separately without copying them into one slice.
pub(crate) fn sum(parts: &[&[u8]]) -> u16 {
    let mut total = 0u32;
    let mut high: Option<u8> = None;
    for part in parts {
        for &byte in *part {
            match high.take() {
                Some(high) => {
                    total = total.wrapping_add(u32::from(u16::from_be_bytes([high, byte])));
                }
                None => high = Some(byte),
            }
        }
    }
    if let Some(high) = high {
        total = total.wrapping_add(u32::from(u16::from_be_bytes([high, 0])));
    }
    while total > 0xffff {
        total = (total & 0xffff) + (total >> 16);
    }
    total as u16
}

/// One's complement of [`sum`]: the value a checksum field carries.
pub(crate) fn checksum(parts: &[&[u8]]) -> u16 {
    !sum(parts)
}

/// Reason why a packet is refused.
///
/// The variants name the field that is wrong, because a frame that is dropped
/// without a word cannot be told apart from a frame that never arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The buffer is shorter than the fixed part of the header it must hold.
    TooShort { needed: usize, found: usize },
    /// A length field names bytes the buffer does not hold.
    Truncated { declared: usize, found: usize },
    /// A length field is below the smallest value the format allows.
    LengthInvalid {
        field: &'static str,
        value: u16,
        min: u16,
    },
    /// A length field disagrees with the bytes given to the caller.
    LengthMismatch { declared: usize, found: usize },
    /// The version of an IPv4 header is not four.
    UnsupportedVersion(u8),
    /// The address lengths of an ARP packet are not the ones of this link.
    UnsupportedAddressLengths { hardware: u8, protocol: u8 },
    /// A field does not fit the bits the wire format gives it.
    FieldTooLarge {
        field: &'static str,
        value: usize,
        max: usize,
    },
    /// A value does not fit the type that carries it on the wire.
    TooLong(usize),
    /// The checksum field does not match the bytes of the header.
    ChecksumMismatch { declared: u16, computed: u16 },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooShort { needed, found } => write!(
                formatter,
                "the buffer holds {found} bytes, the header needs {needed}"
            ),
            Error::Truncated { declared, found } => write!(
                formatter,
                "the length field says {declared} bytes, the buffer holds {found}"
            ),
            Error::LengthInvalid { field, value, min } => {
                write!(formatter, "{field} is {value}, it must be at least {min}")
            }
            Error::LengthMismatch { declared, found } => write!(
                formatter,
                "the length field says {declared} bytes, the buffer holds {found}"
            ),
            Error::UnsupportedVersion(version) => {
                write!(formatter, "the version is {version}, not 4")
            }
            Error::UnsupportedAddressLengths { hardware, protocol } => write!(
                formatter,
                "the address lengths are {hardware} and {protocol} bytes, this link uses 6 and 4"
            ),
            Error::FieldTooLarge { field, value, max } => {
                write!(formatter, "{field} is {value}, it must not exceed {max}")
            }
            Error::TooLong(length) => {
                write!(formatter, "{length} bytes do not fit the length field")
            }
            Error::ChecksumMismatch { declared, computed } => write!(
                formatter,
                "the checksum field is 0x{declared:04x}, the bytes give 0x{computed:04x}"
            ),
        }
    }
}

impl core::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_of_an_odd_length_completes_the_last_word_with_zero() {
        assert_eq!(sum(&[&[0x12, 0x34, 0x56]]), 0x1234 + 0x5600);
    }

    #[test]
    fn sum_carries_across_the_parts_of_a_header() {
        let whole = sum(&[&[0x45, 0x00, 0x00, 0x3c]]);
        let split = sum(&[&[0x45], &[0x00, 0x00, 0x3c]]);
        assert_eq!(whole, split);
    }

    #[test]
    fn checksum_is_the_complement_of_the_sum() {
        let bytes = [0x00, 0x01, 0xf2, 0x03];
        assert_eq!(checksum(&[&bytes]), !sum(&[&bytes]));
    }

    #[test]
    fn an_error_says_what_was_wrong() {
        let error = Error::TooShort {
            needed: 14,
            found: 3,
        };
        assert_eq!(
            error.to_string(),
            "the buffer holds 3 bytes, the header needs 14"
        );
    }
}
