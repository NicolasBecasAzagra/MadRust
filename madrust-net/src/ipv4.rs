// SPDX-License-Identifier: GPL-2.0-only
//! The header of an IPv4 datagram (RFC 791).
//!
//! The header names the source and the destination of the datagram, the
//! protocol it carries, its length and a checksum over the header alone. The
//! header length is counted in 32-bit words, so [`Header::header_length`] holds
//! the number five for the twenty bytes every header has and more when options
//! follow the fixed fields.
//!
//! The checksum is the one's complement of the one's complement sum of the
//! 16-bit words of the header with the checksum field at zero, the same
//! operation UDP uses over its pseudo header. A header whose checksum field is
//! the value [`Header::checksum`] returns sums to `0xffff`, and that is what
//! [`Header::verify_checksum`] checks.

use crate::Error;

/// Version of the protocol this module parses.
pub const VERSION: u8 = 4;

/// Size of the fixed part of a header, in bytes: five 32-bit words.
pub const MIN_HEADER_SIZE: usize = 20;

/// Largest header, in bytes: fifteen 32-bit words, options included.
pub const MAX_HEADER_SIZE: usize = 60;

/// Protocol number of a UDP datagram, the value of the protocol field.
pub const PROTOCOL_UDP: u8 = 17;

/// Protocol number of a TCP segment, the value of the protocol field.
pub const PROTOCOL_TCP: u8 = 6;

/// Offset of a fragment inside its datagram, in eight byte units.
pub const MAX_FRAGMENT_OFFSET: u16 = 0x1fff;

/// Largest value of the three flag bits.
pub const MAX_FLAGS: u8 = 0b111;

/// The fixed fields of an IPv4 header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Version of the protocol, [`VERSION`] here.
    pub version: u8,
    /// Length of the header in 32-bit words, five without options.
    pub header_length: u8,
    /// Type of service, the bits a router reads to pick a queue.
    pub type_of_service: u8,
    /// Length of the whole datagram, header and payload, in bytes.
    pub total_length: u16,
    /// Identification a fragment shares with the other fragments of its
    /// datagram.
    pub identification: u16,
    /// Three flag bits: reserved, do not fragment, more fragments.
    pub flags: u8,
    /// Position of this fragment inside its datagram, in eight byte units.
    pub fragment_offset: u16,
    /// Hops the datagram may still make.
    pub time_to_live: u8,
    /// Protocol carried by the payload, [`PROTOCOL_UDP`] for a UDP datagram.
    pub protocol: u8,
    /// Checksum of the header, computed by [`Header::checksum`].
    pub checksum: u16,
    /// Address of the sender.
    pub source: [u8; 4],
    /// Address of the destination.
    pub destination: [u8; 4],
}

impl Header {
    /// Size of the header in bytes, options included.
    pub const fn header_size(&self) -> usize {
        self.header_length as usize * 4
    }

    /// Take a header out of the bytes of a datagram.
    ///
    /// The buffer must hold the whole header the length field names, so a
    /// datagram that was cut short is refused before any field is read. The
    /// checksum is not verified here: [`Header::verify_checksum`] does that,
    /// because a receiver may want to know what arrived before it refuses it.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < MIN_HEADER_SIZE {
            return Err(Error::TooShort {
                needed: MIN_HEADER_SIZE,
                found: bytes.len(),
            });
        }

        let version = bytes[0] >> 4;
        if version != VERSION {
            return Err(Error::UnsupportedVersion(version));
        }

        let header_length = bytes[0] & 0x0f;
        if header_length < 5 {
            return Err(Error::LengthInvalid {
                field: "the IPv4 header length",
                value: u16::from(header_length),
                min: 5,
            });
        }

        let header_size = usize::from(header_length) * 4;
        if bytes.len() < header_size {
            return Err(Error::TooShort {
                needed: header_size,
                found: bytes.len(),
            });
        }

        let total_length = u16::from_be_bytes([bytes[2], bytes[3]]);
        if total_length < header_size as u16 {
            return Err(Error::LengthInvalid {
                field: "the IPv4 total length",
                value: total_length,
                min: header_size as u16,
            });
        }

        let mut source = [0u8; 4];
        source.copy_from_slice(&bytes[12..16]);
        let mut destination = [0u8; 4];
        destination.copy_from_slice(&bytes[16..20]);

        let flags_fragment = u16::from_be_bytes([bytes[6], bytes[7]]);

        Ok(Self {
            version,
            header_length,
            type_of_service: bytes[1],
            total_length,
            identification: u16::from_be_bytes([bytes[4], bytes[5]]),
            flags: (flags_fragment >> 13) as u8,
            fragment_offset: flags_fragment & MAX_FRAGMENT_OFFSET,
            time_to_live: bytes[8],
            protocol: bytes[9],
            checksum: u16::from_be_bytes([bytes[10], bytes[11]]),
            source,
            destination,
        })
    }

    /// Write the header into the first [`Header::header_size`] bytes of `into`.
    ///
    /// The checksum is written as it is stored in the header: call
    /// [`Header::fill_checksum`] first when the field must be computed.
    pub fn write(&self, into: &mut [u8]) -> Result<(), Error> {
        let (bytes, size) = self.encode()?;
        if into.len() < size {
            return Err(Error::TooShort {
                needed: size,
                found: into.len(),
            });
        }
        into[..size].copy_from_slice(&bytes[..size]);
        Ok(())
    }

    /// The header as the bytes of the wire format, from a zeroed buffer of the
    /// largest header to the size this header occupies.
    fn encode(&self) -> Result<([u8; MAX_HEADER_SIZE], usize), Error> {
        if self.version != VERSION {
            return Err(Error::UnsupportedVersion(self.version));
        }
        if self.header_length < 5 {
            return Err(Error::LengthInvalid {
                field: "the IPv4 header length",
                value: u16::from(self.header_length),
                min: 5,
            });
        }
        if self.header_length > (MAX_HEADER_SIZE / 4) as u8 {
            return Err(Error::FieldTooLarge {
                field: "the IPv4 header length",
                value: usize::from(self.header_length),
                max: MAX_HEADER_SIZE / 4,
            });
        }
        if self.flags > MAX_FLAGS {
            return Err(Error::FieldTooLarge {
                field: "the IPv4 flags",
                value: usize::from(self.flags),
                max: usize::from(MAX_FLAGS),
            });
        }
        if self.fragment_offset > MAX_FRAGMENT_OFFSET {
            return Err(Error::FieldTooLarge {
                field: "the IPv4 fragment offset",
                value: usize::from(self.fragment_offset),
                max: usize::from(MAX_FRAGMENT_OFFSET),
            });
        }

        let size = self.header_size();
        let mut bytes = [0u8; MAX_HEADER_SIZE];
        bytes[0] = (VERSION << 4) | self.header_length;
        bytes[1] = self.type_of_service;
        bytes[2..4].copy_from_slice(&self.total_length.to_be_bytes());
        bytes[4..6].copy_from_slice(&self.identification.to_be_bytes());
        let flags_fragment = (u16::from(self.flags) << 13) | self.fragment_offset;
        bytes[6..8].copy_from_slice(&flags_fragment.to_be_bytes());
        bytes[8] = self.time_to_live;
        bytes[9] = self.protocol;
        bytes[10..12].copy_from_slice(&self.checksum.to_be_bytes());
        bytes[12..16].copy_from_slice(&self.source);
        bytes[16..20].copy_from_slice(&self.destination);
        Ok((bytes, size))
    }

    /// Checksum of the header with the checksum field at zero.
    pub fn checksum(&self) -> Result<u16, Error> {
        let mut header = *self;
        header.checksum = 0;
        let (bytes, size) = header.encode()?;
        Ok(crate::checksum(&[&bytes[..size]]))
    }

    /// Check the checksum field against the bytes of the header.
    ///
    /// A header whose field is the value [`Header::checksum`] returns sums to
    /// `0xffff` over all its words, checksum field included.
    pub fn verify_checksum(&self) -> Result<(), Error> {
        let (bytes, size) = self.encode()?;
        if crate::sum(&[&bytes[..size]]) == 0xffff {
            return Ok(());
        }
        Err(Error::ChecksumMismatch {
            declared: self.checksum,
            computed: self.checksum()?,
        })
    }

    /// Store the checksum of the fields into the checksum field.
    pub fn fill_checksum(&mut self) -> Result<(), Error> {
        self.checksum = self.checksum()?;
        Ok(())
    }
}

/// One header and the payload the header says it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    /// The fixed fields of the header.
    pub header: Header,
    /// The bytes after the header, cut at the total length of the datagram.
    pub payload: &'a [u8],
}

/// Take a datagram apart into its header and its payload.
///
/// The payload stops at the total length of the header, so the padding some
/// links add to a short frame does not leak into the datagram.
pub fn parse(bytes: &[u8]) -> Result<Packet<'_>, Error> {
    let header = Header::parse(bytes)?;
    let end = usize::from(header.total_length);
    if bytes.len() < end {
        return Err(Error::Truncated {
            declared: end,
            found: bytes.len(),
        });
    }

    Ok(Packet {
        header,
        payload: &bytes[header.header_size()..end],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header of twenty bytes with no option and the checksum field at zero,
    /// the example of the checksum computation of RFC 1071.
    const HEADER: [u8; 20] = [
        0x45, // version 4, header length 5
        0x00, // type of service
        0x00, 0x73, // total length
        0x00, 0x00, // identification
        0x40, 0x00, // do not fragment, offset zero
        0x40, // time to live 64
        0x11, // protocol: UDP
        0x00, 0x00, // checksum, to be computed
        0xc0, 0xa8, 0x00, 0x01, // source: 192.168.0.1
        0xc0, 0xa8, 0x00, 0xc7, // destination: 192.168.0.199
    ];

    /// The checksum RFC 1071 gives for [`HEADER`].
    const HEADER_CHECKSUM: u16 = 0xb861;

    fn header() -> Header {
        Header::parse(&HEADER).expect("the header is twenty bytes")
    }

    #[test]
    fn every_field_is_read_where_the_specification_puts_it() {
        let header = header();

        assert_eq!(header.version, VERSION);
        assert_eq!(header.header_length, 5);
        assert_eq!(header.header_size(), MIN_HEADER_SIZE);
        assert_eq!(header.type_of_service, 0);
        assert_eq!(header.total_length, 0x0073);
        assert_eq!(header.identification, 0);
        assert_eq!(header.flags, 0b010);
        assert_eq!(header.fragment_offset, 0);
        assert_eq!(header.time_to_live, 64);
        assert_eq!(header.protocol, PROTOCOL_UDP);
        assert_eq!(header.checksum, 0);
        assert_eq!(header.source, [192, 168, 0, 1]);
        assert_eq!(header.destination, [192, 168, 0, 199]);
    }

    #[test]
    fn the_checksum_of_the_header_is_the_one_of_rfc_1071() {
        assert_eq!(header().checksum(), Ok(HEADER_CHECKSUM));
    }

    #[test]
    fn a_header_carrying_its_checksum_passes_the_check() {
        let mut header = header();
        header.fill_checksum().expect("the fields are valid");

        assert_eq!(header.checksum, HEADER_CHECKSUM);
        assert_eq!(header.verify_checksum(), Ok(()));
        let mut bytes = [0u8; MIN_HEADER_SIZE];
        header.write(&mut bytes).expect("the buffer holds a header");
        assert_eq!(bytes, header_bytes());
    }

    #[test]
    fn a_header_with_a_wrong_checksum_is_refused() {
        let mut header = header();
        header.fill_checksum().expect("the fields are valid");
        header.time_to_live = 63;

        assert_eq!(
            header.verify_checksum(),
            Err(Error::ChecksumMismatch {
                declared: HEADER_CHECKSUM,
                computed: 0xb961,
            })
        );
    }

    fn header_bytes() -> [u8; MIN_HEADER_SIZE] {
        let mut bytes = HEADER;
        bytes[10..12].copy_from_slice(&HEADER_CHECKSUM.to_be_bytes());
        bytes
    }

    #[test]
    fn the_header_survives_the_round_trip() {
        let mut header = header();
        header.fill_checksum().expect("the fields are valid");
        let mut bytes = [0u8; MIN_HEADER_SIZE];
        header.write(&mut bytes).expect("the buffer holds a header");

        assert_eq!(bytes, header_bytes());
        assert_eq!(Header::parse(&bytes), Ok(header));
    }

    #[test]
    fn the_payload_stops_at_the_total_length() {
        let mut bytes = [0xaa; 32];
        bytes[..MIN_HEADER_SIZE].copy_from_slice(&HEADER);
        bytes[2..4].copy_from_slice(&24u16.to_be_bytes());

        let packet = parse(&bytes).expect("the datagram is twenty four bytes");

        assert_eq!(packet.header.total_length, 24);
        assert_eq!(packet.payload, &[0xaa; 4]);
    }

    #[test]
    fn a_datagram_shorter_than_its_header_is_refused() {
        assert_eq!(
            Header::parse(&[0u8; MIN_HEADER_SIZE - 1]),
            Err(Error::TooShort {
                needed: MIN_HEADER_SIZE,
                found: MIN_HEADER_SIZE - 1,
            })
        );
    }

    #[test]
    fn a_header_with_options_is_refused_when_the_buffer_is_short() {
        let mut bytes = HEADER;
        bytes[0] = 0x47;

        assert_eq!(
            Header::parse(&bytes),
            Err(Error::TooShort {
                needed: 28,
                found: MIN_HEADER_SIZE,
            })
        );
    }

    #[test]
    fn a_version_other_than_four_is_refused() {
        let mut bytes = HEADER;
        bytes[0] = 0x65;

        assert_eq!(Header::parse(&bytes), Err(Error::UnsupportedVersion(6)));
    }

    #[test]
    fn a_header_length_below_five_words_is_refused() {
        let mut bytes = HEADER;
        bytes[0] = 0x44;

        assert_eq!(
            Header::parse(&bytes),
            Err(Error::LengthInvalid {
                field: "the IPv4 header length",
                value: 4,
                min: 5,
            })
        );
    }

    #[test]
    fn a_datagram_cut_before_its_total_length_is_refused() {
        let mut bytes = [0u8; 24];
        bytes[..MIN_HEADER_SIZE].copy_from_slice(&HEADER);
        bytes[2] = 0x00;
        bytes[3] = 0x40;

        assert_eq!(
            parse(&bytes),
            Err(Error::Truncated {
                declared: 64,
                found: 24,
            })
        );
    }
}
