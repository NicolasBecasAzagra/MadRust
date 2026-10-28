// SPDX-License-Identifier: GPL-2.0-only
//! The header of a UDP datagram (RFC 768).
//!
//! The header is four half words: the port of the source, the port of the
//! destination, the length of the datagram, header included, and a checksum.
//! The length is the only size the datagram has on the wire, so the receiver
//! cuts the payload there and not at the end of the frame that carried it.
//!
//! The checksum covers a pseudo header, the header and the payload. The pseudo
//! header holds the two IPv4 addresses, the protocol number, the length of the
//! datagram and a zero word, so a datagram delivered to the wrong address is
//! refused. A checksum field of zero means the sender did not compute it: UDP
//! allows that, and then the datagram is not checked at all.

use crate::Error;

/// Size of the header: four half words.
pub const HEADER_SIZE: usize = 8;

/// Protocol number of UDP, the value of the protocol field of IPv4.
pub const PROTOCOL: u8 = 17;

/// Size of the pseudo header the checksum covers first.
pub const PSEUDO_HEADER_SIZE: usize = 12;

/// Offset of the checksum half word inside the datagram.
const CHECKSUM_OFFSET: usize = 6;

/// The four fields of a UDP header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Port of the sender.
    pub source_port: u16,
    /// Port of the receiver.
    pub destination_port: u16,
    /// Length of the whole datagram, header and payload, in bytes.
    pub length: u16,
    /// Checksum of the datagram, zero when the sender did not compute one.
    pub checksum: u16,
}

impl Header {
    /// Take a header out of the first [`HEADER_SIZE`] bytes of a datagram.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < HEADER_SIZE {
            return Err(Error::TooShort {
                needed: HEADER_SIZE,
                found: bytes.len(),
            });
        }

        Ok(Self {
            source_port: u16::from_be_bytes([bytes[0], bytes[1]]),
            destination_port: u16::from_be_bytes([bytes[2], bytes[3]]),
            length: u16::from_be_bytes([bytes[4], bytes[5]]),
            checksum: u16::from_be_bytes([bytes[6], bytes[7]]),
        })
    }

    /// Write the header into the first [`HEADER_SIZE`] bytes of `into`.
    pub fn write(&self, into: &mut [u8]) -> Result<(), Error> {
        if into.len() < HEADER_SIZE {
            return Err(Error::TooShort {
                needed: HEADER_SIZE,
                found: into.len(),
            });
        }

        into[0..2].copy_from_slice(&self.source_port.to_be_bytes());
        into[2..4].copy_from_slice(&self.destination_port.to_be_bytes());
        into[4..6].copy_from_slice(&self.length.to_be_bytes());
        into[6..8].copy_from_slice(&self.checksum.to_be_bytes());
        Ok(())
    }

    /// The header as the bytes of the wire format.
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut bytes = [0u8; HEADER_SIZE];
        // The array has exactly the size `write` needs, so this cannot fail.
        let _ = self.write(&mut bytes);
        bytes
    }
}

/// One header and the payload the header says it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Datagram<'a> {
    /// The four fields of the header.
    pub header: Header,
    /// The bytes after the header, cut at the length of the datagram.
    pub payload: &'a [u8],
}

/// Take a datagram apart into its header and its payload.
///
/// The payload stops at the length field, so the zero padding of the frame that
/// carried a short datagram does not leak into it.
pub fn parse(bytes: &[u8]) -> Result<Datagram<'_>, Error> {
    let header = Header::parse(bytes)?;
    if header.length < HEADER_SIZE as u16 {
        return Err(Error::LengthInvalid {
            field: "the UDP length",
            value: header.length,
            min: HEADER_SIZE as u16,
        });
    }

    let end = usize::from(header.length);
    if bytes.len() < end {
        return Err(Error::Truncated {
            declared: end,
            found: bytes.len(),
        });
    }

    Ok(Datagram {
        header,
        payload: &bytes[HEADER_SIZE..end],
    })
}

/// Bytes of the pseudo header the checksum covers before the datagram.
fn pseudo_header(source: [u8; 4], destination: [u8; 4], length: u16) -> [u8; PSEUDO_HEADER_SIZE] {
    let mut bytes = [0u8; PSEUDO_HEADER_SIZE];
    bytes[0..4].copy_from_slice(&source);
    bytes[4..8].copy_from_slice(&destination);
    bytes[8] = 0;
    bytes[9] = PROTOCOL;
    bytes[10..12].copy_from_slice(&length.to_be_bytes());
    bytes
}

/// Check that `datagram` holds the length its header names.
fn checked_length(datagram: &[u8]) -> Result<(Header, u16), Error> {
    let header = Header::parse(datagram)?;
    if header.length < HEADER_SIZE as u16 {
        return Err(Error::LengthInvalid {
            field: "the UDP length",
            value: header.length,
            min: HEADER_SIZE as u16,
        });
    }
    let length = u16::try_from(datagram.len()).map_err(|_| Error::TooLong(datagram.len()))?;
    if header.length != length {
        return Err(Error::LengthMismatch {
            declared: usize::from(header.length),
            found: datagram.len(),
        });
    }
    Ok((header, length))
}

/// Checksum of a whole datagram, header and payload, between two addresses.
///
/// `datagram` must be exactly the bytes the length field of the header names,
/// header included; a frame that padded a short datagram is cut first.
///
/// The value returned is the one the checksum field must carry. When the sum
/// comes out at zero the field must hold `0xffff` instead, because a zero field
/// tells the receiver that the sender did not check the datagram at all.
pub fn checksum(source: [u8; 4], destination: [u8; 4], datagram: &[u8]) -> Result<u16, Error> {
    let (_, length) = checked_length(datagram)?;
    let pseudo = pseudo_header(source, destination, length);
    let sum = crate::sum(&[
        &pseudo,
        &datagram[..CHECKSUM_OFFSET],
        &[0, 0],
        &datagram[CHECKSUM_OFFSET + 2..],
    ]);
    let checksum = !sum;
    Ok(if checksum == 0 { 0xffff } else { checksum })
}

/// Check the checksum field of a received datagram.
///
/// A field of zero means that the sender did not compute a checksum, and UDP
/// says such a datagram is not checked: it passes. Otherwise the pseudo header,
/// the header and the payload must sum to `0xffff`.
pub fn verify(source: [u8; 4], destination: [u8; 4], datagram: &[u8]) -> Result<(), Error> {
    let parsed = parse(datagram)?;
    if parsed.header.checksum == 0 {
        return Ok(());
    }

    let length = parsed.header.length;
    let pseudo = pseudo_header(source, destination, length);
    let end = usize::from(length);
    if crate::sum(&[&pseudo, &datagram[..end]]) == 0xffff {
        return Ok(());
    }

    Err(Error::ChecksumMismatch {
        declared: parsed.header.checksum,
        computed: checksum(source, destination, &datagram[..end])?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipv4;

    const SOURCE: [u8; 4] = [192, 168, 0, 1];
    const DESTINATION: [u8; 4] = [192, 168, 0, 2];
    const PAYLOAD: &[u8] = b"Hello, UDP!";

    /// Source port 1234, destination port 5678, length 19 and a checksum field
    /// at zero, followed by a payload of eleven bytes.
    const DATAGRAM: [u8; 19] = [
        0x04, 0xd2, // source port 1234
        0x16, 0x2e, // destination port 5678
        0x00, 0x13, // length 19
        0x00, 0x00, // checksum, to be computed
        b'H', b'e', b'l', b'l', b'o', b',', b' ', b'U', b'D', b'P', b'!',
    ];

    /// The value the checksum field carries for [`DATAGRAM`] between
    /// [`SOURCE`] and [`DESTINATION`].
    const DATAGRAM_CHECKSUM: u16 = 0xb9d0;

    #[test]
    fn the_pseudo_header_is_the_twelve_bytes_of_the_specification() {
        assert_eq!(
            pseudo_header(SOURCE, DESTINATION, 19),
            [
                192, 168, 0, 1, // source
                192, 168, 0, 2,  // destination
                0,  // zero
                17, // protocol: UDP
                0, 19, // length
            ]
        );
    }

    #[test]
    fn every_field_is_read_where_the_specification_puts_it() {
        let header = Header::parse(&DATAGRAM).expect("the header is eight bytes");

        assert_eq!(header.source_port, 1234);
        assert_eq!(header.destination_port, 5678);
        assert_eq!(header.length, 19);
        assert_eq!(header.checksum, 0);
    }

    #[test]
    fn the_checksum_of_the_datagram_is_the_computed_one() {
        assert_eq!(
            checksum(SOURCE, DESTINATION, &DATAGRAM),
            Ok(DATAGRAM_CHECKSUM)
        );
    }

    #[test]
    fn a_datagram_carrying_its_checksum_passes_the_check() {
        let mut datagram = DATAGRAM;
        datagram[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 2]
            .copy_from_slice(&DATAGRAM_CHECKSUM.to_be_bytes());

        assert_eq!(verify(SOURCE, DESTINATION, &datagram), Ok(()));
        assert_eq!(
            checksum(SOURCE, DESTINATION, &datagram),
            Ok(DATAGRAM_CHECKSUM)
        );
    }

    #[test]
    fn a_datagram_of_another_address_is_refused() {
        let mut datagram = DATAGRAM;
        datagram[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 2]
            .copy_from_slice(&DATAGRAM_CHECKSUM.to_be_bytes());

        assert_eq!(
            verify(SOURCE, [192, 168, 0, 3], &datagram),
            Err(Error::ChecksumMismatch {
                declared: DATAGRAM_CHECKSUM,
                computed: 0xb9cf,
            })
        );
    }

    /// A datagram whose sum comes out at zero: its field must hold `0xffff`,
    /// because a field of zero would say that nothing was checked.
    const ZERO_DATAGRAM: [u8; 10] = [
        0x04, 0xd2, // source port 1234
        0x16, 0x2e, // destination port 5678
        0x00, 0x0a, // length 10
        0x00, 0x00, // checksum, to be computed
        0x63, 0x86, // payload
    ];

    #[test]
    fn a_sum_of_zero_is_written_as_all_ones() {
        assert_eq!(checksum(SOURCE, DESTINATION, &ZERO_DATAGRAM), Ok(0xffff));
    }

    #[test]
    fn a_datagram_with_the_all_ones_checksum_passes_the_check() {
        let mut datagram = ZERO_DATAGRAM;
        datagram[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 2].copy_from_slice(&0xffffu16.to_be_bytes());

        assert_eq!(verify(SOURCE, DESTINATION, &datagram), Ok(()));
    }

    #[test]
    fn a_datagram_without_a_checksum_is_not_checked() {
        // The checksum field is zero and the bytes are wrong for the addresses,
        // yet the datagram passes: UDP says a zero field means no checksum.
        assert_eq!(verify([0, 0, 0, 0], DESTINATION, &DATAGRAM), Ok(()));
    }

    #[test]
    fn the_payload_stops_at_the_length() {
        let mut bytes = [0xaa; 32];
        bytes[..DATAGRAM.len()].copy_from_slice(&DATAGRAM);

        let datagram = parse(&bytes).expect("the datagram is nineteen bytes");

        assert_eq!(datagram.header.length, 19);
        assert_eq!(datagram.payload, PAYLOAD);
    }

    #[test]
    fn a_datagram_shorter_than_the_header_is_refused() {
        assert_eq!(
            Header::parse(&[0u8; HEADER_SIZE - 1]),
            Err(Error::TooShort {
                needed: HEADER_SIZE,
                found: HEADER_SIZE - 1,
            })
        );
    }

    #[test]
    fn a_length_below_the_header_is_refused() {
        let mut datagram = DATAGRAM;
        datagram[4..6].copy_from_slice(&4u16.to_be_bytes());

        assert_eq!(
            parse(&datagram),
            Err(Error::LengthInvalid {
                field: "the UDP length",
                value: 4,
                min: 8,
            })
        );
    }

    #[test]
    fn a_datagram_cut_before_its_length_is_refused() {
        let mut datagram = DATAGRAM;
        datagram[4..6].copy_from_slice(&40u16.to_be_bytes());

        assert_eq!(
            parse(&datagram),
            Err(Error::Truncated {
                declared: 40,
                found: 19,
            })
        );
    }

    #[test]
    fn a_checksum_over_a_length_that_is_not_the_buffer_is_refused() {
        let mut datagram = DATAGRAM;
        datagram[4..6].copy_from_slice(&18u16.to_be_bytes());

        assert_eq!(
            checksum(SOURCE, DESTINATION, &datagram),
            Err(Error::LengthMismatch {
                declared: 18,
                found: 19,
            })
        );
    }

    #[test]
    fn the_protocol_number_is_the_one_of_ipv4() {
        assert_eq!(PROTOCOL, ipv4::PROTOCOL_UDP);
    }
}
