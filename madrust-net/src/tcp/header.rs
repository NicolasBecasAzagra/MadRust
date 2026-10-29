// SPDX-License-Identifier: GPL-2.0-only
//! The header of a TCP segment (RFC 9293, formerly RFC 793).
//!
//! The header is five 32-bit words and more when options follow: the two
//! ports, the sequence number of the first byte the sender names, the
//! acknowledgement number of the last byte the sender received, the length of
//! the header in 32-bit words, the flags, the window, the checksum and the
//! urgent pointer.
//!
//! The data offset counts 32-bit words, so a header without options says five.
//! This crate does not model the options: a parsed header keeps its offset and
//! the payload starts after it, and a header this crate writes has no option
//! byte. The three low bits of the offset byte, the reserved bits and the NS
//! flag of RFC 3540, are read as zero and written as zero.
//!
//! The checksum covers a pseudo header, the header and the payload. The
//! pseudo header holds the two IPv4 addresses, the protocol number, the length
//! of the segment and a zero byte, so a segment delivered to the wrong address
//! is refused. TCP has no value that means "not computed": a wrong checksum is
//! always an error.

use crate::Error;

/// Size of a header without options: five 32-bit words.
pub const MIN_HEADER_SIZE: usize = 20;

/// Largest header: fifteen 32-bit words, options included.
pub const MAX_HEADER_SIZE: usize = 60;

/// Protocol number of TCP, the value of the protocol field of IPv4.
pub const PROTOCOL: u8 = 6;

/// Size of the pseudo header the checksum covers first.
pub const PSEUDO_HEADER_SIZE: usize = 12;

/// Smallest data offset: five 32-bit words.
const MIN_DATA_OFFSET: u8 = 5;

/// Largest data offset the four bits of the offset byte carry.
const MAX_DATA_OFFSET: u8 = 15;

/// The six flags and nothing else: the reserved bits are not part of this
/// type.
const FLAG_MASK: u8 = 0x3f;

/// Offset of the checksum half word inside a segment.
const CHECKSUM_OFFSET: usize = 16;

/// The six flags of a TCP header.
///
/// The type is a set: a header carries any combination of them, and
/// [`Flags::contains`] answers whether one of them is set. The order of the
/// bits is the order of the byte of the wire format, so [`Flags::bits`] is the
/// value the offset byte holds below the data offset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(u8);

impl Flags {
    /// No flag at all.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// No more data from the sender.
    pub const FIN: Self = Self(0x01);

    /// The sender names its first sequence number.
    pub const SYN: Self = Self(0x02);

    /// The connection is refused, or is gone.
    pub const RST: Self = Self(0x04);

    /// The segment carries data the receiver should pass on at once.
    pub const PSH: Self = Self(0x08);

    /// The acknowledgement field is meaningful.
    pub const ACK: Self = Self(0x10);

    /// The urgent pointer is meaningful.
    pub const URG: Self = Self(0x20);

    /// The flags as the six bits of the header.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every flag of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no flag is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Set every flag of `other`.
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Clear every flag of `other`.
    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

impl core::ops::BitOr for Flags {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// The fields of a TCP header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Port of the sender.
    pub source_port: u16,
    /// Port of the receiver.
    pub destination_port: u16,
    /// Sequence number of the first byte of the payload, or of the SYN or FIN
    /// the segment carries.
    pub sequence: u32,
    /// Sequence number of the next byte the sender expects, minus one; it is
    /// meaningful only with [`Flags::ACK`].
    pub acknowledgement: u32,
    /// Length of the header in 32-bit words, five without options.
    pub data_offset: u8,
    /// The six flags of the segment.
    pub flags: Flags,
    /// Bytes the sender is willing to receive.
    pub window: u16,
    /// Checksum of the pseudo header, the header and the payload.
    pub checksum: u16,
    /// Offset of urgent data, meaningful only with [`Flags::URG`].
    pub urgent_pointer: u16,
}

impl Header {
    /// A header without options and with the checksum field at zero.
    pub const fn new(
        source_port: u16,
        destination_port: u16,
        sequence: u32,
        acknowledgement: u32,
        flags: Flags,
        window: u16,
    ) -> Self {
        Self {
            source_port,
            destination_port,
            sequence,
            acknowledgement,
            data_offset: MIN_DATA_OFFSET,
            flags,
            window,
            checksum: 0,
            urgent_pointer: 0,
        }
    }

    /// Size of the header in bytes, options included.
    pub const fn header_size(&self) -> usize {
        self.data_offset as usize * 4
    }

    /// Take a header out of the bytes of a segment.
    ///
    /// The buffer must hold the whole header the data offset names, so a
    /// segment that was cut short is refused before any field is read. The
    /// checksum is not verified here: [`Header::verify_checksum`] does that,
    /// because a receiver may want to know what arrived before it refuses it.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < MIN_HEADER_SIZE {
            return Err(Error::TooShort {
                needed: MIN_HEADER_SIZE,
                found: bytes.len(),
            });
        }

        let data_offset = bytes[12] >> 4;
        if data_offset < MIN_DATA_OFFSET {
            return Err(Error::LengthInvalid {
                field: "the TCP data offset",
                value: u16::from(data_offset),
                min: u16::from(MIN_DATA_OFFSET),
            });
        }

        let header_size = usize::from(data_offset) * 4;
        if bytes.len() < header_size {
            return Err(Error::TooShort {
                needed: header_size,
                found: bytes.len(),
            });
        }

        Ok(Self {
            source_port: u16::from_be_bytes([bytes[0], bytes[1]]),
            destination_port: u16::from_be_bytes([bytes[2], bytes[3]]),
            sequence: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            acknowledgement: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            data_offset,
            flags: Flags(bytes[13] & FLAG_MASK),
            window: u16::from_be_bytes([bytes[14], bytes[15]]),
            checksum: u16::from_be_bytes([bytes[16], bytes[17]]),
            urgent_pointer: u16::from_be_bytes([bytes[18], bytes[19]]),
        })
    }

    /// Write the header into the first [`Header::header_size`] bytes of
    /// `into`.
    ///
    /// The checksum is written as it is stored in the header: call
    /// [`Header::fill_checksum`] first when the field must be computed. The
    /// option bytes, if the data offset names any, are written as zero.
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
        if self.data_offset < MIN_DATA_OFFSET {
            return Err(Error::LengthInvalid {
                field: "the TCP data offset",
                value: u16::from(self.data_offset),
                min: u16::from(MIN_DATA_OFFSET),
            });
        }
        if self.data_offset > MAX_DATA_OFFSET {
            return Err(Error::FieldTooLarge {
                field: "the TCP data offset",
                value: usize::from(self.data_offset),
                max: usize::from(MAX_DATA_OFFSET),
            });
        }
        if self.flags.bits() & !FLAG_MASK != 0 {
            return Err(Error::FieldTooLarge {
                field: "the TCP flags",
                value: usize::from(self.flags.bits()),
                max: usize::from(FLAG_MASK),
            });
        }

        let size = self.header_size();
        let mut bytes = [0u8; MAX_HEADER_SIZE];
        bytes[0..2].copy_from_slice(&self.source_port.to_be_bytes());
        bytes[2..4].copy_from_slice(&self.destination_port.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.sequence.to_be_bytes());
        bytes[8..12].copy_from_slice(&self.acknowledgement.to_be_bytes());
        bytes[12] = self.data_offset << 4;
        bytes[13] = self.flags.bits();
        bytes[14..16].copy_from_slice(&self.window.to_be_bytes());
        bytes[16..18].copy_from_slice(&self.checksum.to_be_bytes());
        bytes[18..20].copy_from_slice(&self.urgent_pointer.to_be_bytes());
        Ok((bytes, size))
    }

    /// Checksum of the header and `payload` between two addresses, with the
    /// checksum field at zero.
    pub fn checksum(
        &self,
        source: [u8; 4],
        destination: [u8; 4],
        payload: &[u8],
    ) -> Result<u16, Error> {
        let mut header = *self;
        header.checksum = 0;
        let (bytes, size) = header.encode()?;
        let length = segment_length(size, payload.len())?;
        let pseudo = pseudo_header(source, destination, length);
        let summed = crate::sum(&[
            &pseudo,
            &bytes[..CHECKSUM_OFFSET],
            &[0, 0],
            &bytes[CHECKSUM_OFFSET + 2..size],
            payload,
        ]);
        Ok(!summed)
    }

    /// Check the checksum field against the bytes of the header and the
    /// payload.
    ///
    /// A header whose field is the value [`Header::checksum`] returns sums to
    /// `0xffff` over the pseudo header, all the words of the header and the
    /// payload.
    pub fn verify_checksum(
        &self,
        source: [u8; 4],
        destination: [u8; 4],
        payload: &[u8],
    ) -> Result<(), Error> {
        let (bytes, size) = self.encode()?;
        let length = segment_length(size, payload.len())?;
        let pseudo = pseudo_header(source, destination, length);
        if crate::sum(&[&pseudo, &bytes[..size], payload]) == 0xffff {
            return Ok(());
        }
        Err(Error::ChecksumMismatch {
            declared: self.checksum,
            computed: self.checksum(source, destination, payload)?,
        })
    }

    /// Store the checksum of the fields and the payload into the checksum
    /// field.
    pub fn fill_checksum(
        &mut self,
        source: [u8; 4],
        destination: [u8; 4],
        payload: &[u8],
    ) -> Result<(), Error> {
        self.checksum = self.checksum(source, destination, payload)?;
        Ok(())
    }
}

/// One header and the payload the data offset says follows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment<'a> {
    /// The fields of the header.
    pub header: Header,
    /// The bytes after the header, up to the end of the buffer.
    pub payload: &'a [u8],
}

impl<'a> Segment<'a> {
    /// Take a segment apart into its header and its payload.
    ///
    /// The payload runs from the end of the header to the end of the buffer,
    /// because TCP has no length field: the length of the datagram of IPv4
    /// cuts it, so the caller passes exactly the bytes of that datagram.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        let header = Header::parse(bytes)?;
        Ok(Self {
            header,
            payload: &bytes[header.header_size()..],
        })
    }

    /// Write the header and the payload into `into`, and return how many bytes
    /// were written.
    pub fn write(&self, into: &mut [u8]) -> Result<usize, Error> {
        let size = self.header.header_size() + self.payload.len();
        if into.len() < size {
            return Err(Error::TooShort {
                needed: size,
                found: into.len(),
            });
        }
        self.header.write(into)?;
        into[self.header.header_size()..size].copy_from_slice(self.payload);
        Ok(size)
    }

    /// Checksum of the segment between two addresses, with the checksum field
    /// at zero.
    pub fn checksum(&self, source: [u8; 4], destination: [u8; 4]) -> Result<u16, Error> {
        self.header.checksum(source, destination, self.payload)
    }

    /// Store the checksum of the segment into the checksum field.
    pub fn fill_checksum(&mut self, source: [u8; 4], destination: [u8; 4]) -> Result<(), Error> {
        self.header.fill_checksum(source, destination, self.payload)
    }

    /// Check the checksum field of the segment.
    pub fn verify_checksum(&self, source: [u8; 4], destination: [u8; 4]) -> Result<(), Error> {
        self.header
            .verify_checksum(source, destination, self.payload)
    }
}

/// Bytes of the pseudo header the checksum covers before the segment.
fn pseudo_header(source: [u8; 4], destination: [u8; 4], length: u16) -> [u8; PSEUDO_HEADER_SIZE] {
    let mut bytes = [0u8; PSEUDO_HEADER_SIZE];
    bytes[0..4].copy_from_slice(&source);
    bytes[4..8].copy_from_slice(&destination);
    bytes[8] = 0;
    bytes[9] = PROTOCOL;
    bytes[10..12].copy_from_slice(&length.to_be_bytes());
    bytes
}

/// Length of a segment, header and payload, as the pseudo header carries it.
fn segment_length(header: usize, payload: usize) -> Result<u16, Error> {
    let length = header.saturating_add(payload);
    u16::try_from(length).map_err(|_| Error::TooLong(length))
}

/// Take a segment apart into its header and its payload.
pub fn parse(bytes: &[u8]) -> Result<Segment<'_>, Error> {
    Segment::parse(bytes)
}

/// Checksum of the bytes of a whole segment, header and payload, between two
/// addresses.
///
/// `segment` must be exactly the bytes of the TCP segment: the length of the
/// datagram of IPv4 cuts it, and the checksum covers that length, not the
/// padding some links add to a short frame.
///
/// The value returned is the one the checksum field must carry. Unlike UDP,
/// zero is an ordinary value here: TCP has no "not computed" case.
pub fn checksum(source: [u8; 4], destination: [u8; 4], segment: &[u8]) -> Result<u16, Error> {
    Header::parse(segment)?;
    let length = u16::try_from(segment.len()).map_err(|_| Error::TooLong(segment.len()))?;
    let pseudo = pseudo_header(source, destination, length);
    let sum = crate::sum(&[
        &pseudo,
        &segment[..CHECKSUM_OFFSET],
        &[0, 0],
        &segment[CHECKSUM_OFFSET + 2..],
    ]);
    Ok(!sum)
}

/// Check the checksum field of a received segment.
///
/// The pseudo header and every byte of the segment must sum to `0xffff`,
/// checksum field included. TCP has no value that skips the check, so a wrong
/// field is always refused.
pub fn verify(source: [u8; 4], destination: [u8; 4], segment: &[u8]) -> Result<(), Error> {
    let header = Header::parse(segment)?;
    let length = u16::try_from(segment.len()).map_err(|_| Error::TooLong(segment.len()))?;
    let pseudo = pseudo_header(source, destination, length);
    if crate::sum(&[&pseudo, segment]) == 0xffff {
        return Ok(());
    }
    Err(Error::ChecksumMismatch {
        declared: header.checksum,
        computed: checksum(source, destination, segment)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipv4;

    const SOURCE: [u8; 4] = [192, 168, 0, 1];
    const DESTINATION: [u8; 4] = [192, 168, 0, 2];
    const PAYLOAD: &[u8] = b"Hello, TCP!";

    /// Source port 1234, destination port 80, sequence 1, acknowledgement 0, a
    /// data offset of five words, the SYN flag, window 65535, a checksum field
    /// at zero and an urgent pointer at zero.
    const SYN: [u8; MIN_HEADER_SIZE] = [
        0x04, 0xd2, // source port 1234
        0x00, 0x50, // destination port 80
        0x00, 0x00, 0x00, 0x01, // sequence 1
        0x00, 0x00, 0x00, 0x00, // acknowledgement 0
        0x50, // data offset 5, reserved bits zero
        0x02, // SYN
        0xff, 0xff, // window 65535
        0x00, 0x00, // checksum, to be computed
        0x00, 0x00, // urgent pointer
    ];

    /// The checksum of [`SYN`] between [`SOURCE`] and [`DESTINATION`], which
    /// `tcpdump` reports as correct for the same header.
    const SYN_CHECKSUM: u16 = 0x296c;

    /// The checksum of a header carrying [`PAYLOAD`] after the same fields,
    /// with the PSH and ACK flags, which `tcpdump` reports as correct too.
    const PAYLOAD_CHECKSUM: u16 = 0x80a8;

    fn syn() -> Header {
        Header::parse(&SYN).expect("the header is twenty bytes")
    }

    fn syn_bytes() -> [u8; MIN_HEADER_SIZE] {
        let mut bytes = SYN;
        bytes[16..18].copy_from_slice(&SYN_CHECKSUM.to_be_bytes());
        bytes
    }

    #[test]
    fn the_pseudo_header_is_the_twelve_bytes_of_the_specification() {
        assert_eq!(
            pseudo_header(SOURCE, DESTINATION, 20),
            [
                192, 168, 0, 1, // source
                192, 168, 0, 2, // destination
                0, // zero
                6, // protocol: TCP
                0, 20, // length
            ]
        );
    }

    #[test]
    fn every_field_is_read_where_the_specification_puts_it() {
        let header = syn();

        assert_eq!(header.source_port, 1234);
        assert_eq!(header.destination_port, 80);
        assert_eq!(header.sequence, 1);
        assert_eq!(header.acknowledgement, 0);
        assert_eq!(header.data_offset, 5);
        assert_eq!(header.header_size(), MIN_HEADER_SIZE);
        assert_eq!(header.flags, Flags::SYN);
        assert_eq!(header.window, 65535);
        assert_eq!(header.checksum, 0);
        assert_eq!(header.urgent_pointer, 0);
    }

    #[test]
    fn the_flags_are_the_six_bits_of_the_header() {
        let flags = Flags::FIN | Flags::SYN | Flags::RST | Flags::PSH | Flags::ACK | Flags::URG;

        assert_eq!(flags.bits(), FLAG_MASK);
        assert!(flags.contains(Flags::ACK));
        assert!(flags.contains(Flags::FIN | Flags::ACK));
        assert!(!Flags::empty().contains(Flags::SYN));
        assert!(Flags::empty().is_empty());

        let mut mutating = Flags::SYN;
        mutating.insert(Flags::ACK);
        assert_eq!(mutating, Flags::SYN | Flags::ACK);
        mutating.remove(Flags::SYN);
        assert_eq!(mutating, Flags::ACK);
    }

    #[test]
    fn the_checksum_of_a_syn_is_the_one_the_fields_give() {
        assert_eq!(syn().checksum(SOURCE, DESTINATION, &[]), Ok(SYN_CHECKSUM));
        assert_eq!(checksum(SOURCE, DESTINATION, &SYN), Ok(SYN_CHECKSUM));
    }

    #[test]
    fn the_checksum_of_a_segment_with_a_payload_covers_the_payload() {
        let mut header = Header::new(1234, 80, 1, 0, Flags::PSH | Flags::ACK, 65535);
        assert_eq!(
            header.checksum(SOURCE, DESTINATION, PAYLOAD),
            Ok(PAYLOAD_CHECKSUM)
        );
        header
            .fill_checksum(SOURCE, DESTINATION, PAYLOAD)
            .expect("the fields are valid");
        assert_eq!(header.checksum, PAYLOAD_CHECKSUM);
        assert_eq!(header.verify_checksum(SOURCE, DESTINATION, PAYLOAD), Ok(()));
    }

    #[test]
    fn a_segment_carrying_its_checksum_passes_the_check() {
        let bytes = syn_bytes();

        assert_eq!(verify(SOURCE, DESTINATION, &bytes), Ok(()));
        assert_eq!(Header::parse(&bytes).unwrap().checksum, SYN_CHECKSUM);
    }

    #[test]
    fn a_segment_of_another_address_is_refused() {
        assert_eq!(
            verify(SOURCE, [192, 168, 0, 3], &syn_bytes()),
            Err(Error::ChecksumMismatch {
                declared: SYN_CHECKSUM,
                computed: 0x296b,
            })
        );
    }

    #[test]
    fn a_segment_with_a_wrong_checksum_is_refused() {
        let mut bytes = syn_bytes();
        bytes[4..8].copy_from_slice(&2u32.to_be_bytes());

        assert_eq!(
            verify(SOURCE, DESTINATION, &bytes),
            Err(Error::ChecksumMismatch {
                declared: SYN_CHECKSUM,
                computed: checksum(SOURCE, DESTINATION, &bytes).unwrap(),
            })
        );
    }

    #[test]
    fn the_header_survives_the_round_trip() {
        let mut header = syn();
        header.fill_checksum(SOURCE, DESTINATION, &[]).unwrap();
        let mut bytes = [0u8; MIN_HEADER_SIZE];
        header.write(&mut bytes).expect("the buffer holds a header");

        assert_eq!(bytes, syn_bytes());
        assert_eq!(Header::parse(&bytes), Ok(header));
    }

    #[test]
    fn the_window_and_the_urgent_pointer_are_read_too() {
        let mut bytes = SYN;
        bytes[14..16].copy_from_slice(&1024u16.to_be_bytes());
        bytes[13] = (Flags::ACK | Flags::URG).bits();
        bytes[18..20].copy_from_slice(&7u16.to_be_bytes());

        let header = Header::parse(&bytes).expect("the header is twenty bytes");

        assert_eq!(header.window, 1024);
        assert_eq!(header.flags, Flags::ACK | Flags::URG);
        assert_eq!(header.urgent_pointer, 7);
    }

    #[test]
    fn the_payload_starts_after_the_header_the_data_offset_names() {
        let mut bytes = [0u8; MIN_HEADER_SIZE + 4 + PAYLOAD.len()];
        bytes[..MIN_HEADER_SIZE].copy_from_slice(&SYN);
        bytes[12] = 6 << 4;
        bytes[MIN_HEADER_SIZE + 4..].copy_from_slice(PAYLOAD);

        let segment = parse(&bytes).expect("the data offset names 24 bytes");

        assert_eq!(segment.header.data_offset, 6);
        assert_eq!(segment.payload, PAYLOAD);
    }

    #[test]
    fn the_header_and_the_payload_are_written_where_they_belong() {
        let segment = Segment {
            header: Header::new(1234, 80, 1, 0, Flags::PSH | Flags::ACK, 65535),
            payload: PAYLOAD,
        };
        let mut bytes = [0u8; MIN_HEADER_SIZE + PAYLOAD.len()];

        assert_eq!(
            segment.write(&mut bytes),
            Ok(MIN_HEADER_SIZE + PAYLOAD.len())
        );
        assert_eq!(&bytes[MIN_HEADER_SIZE..], PAYLOAD);
        assert_eq!(
            Header::parse(&bytes).unwrap().flags,
            Flags::PSH | Flags::ACK
        );
    }

    #[test]
    fn a_segment_shorter_than_the_header_is_refused() {
        assert_eq!(
            Header::parse(&[0u8; MIN_HEADER_SIZE - 1]),
            Err(Error::TooShort {
                needed: MIN_HEADER_SIZE,
                found: MIN_HEADER_SIZE - 1,
            })
        );
    }

    #[test]
    fn a_data_offset_below_five_words_is_refused() {
        let mut bytes = SYN;
        bytes[12] = 4 << 4;

        assert_eq!(
            Header::parse(&bytes),
            Err(Error::LengthInvalid {
                field: "the TCP data offset",
                value: 4,
                min: 5,
            })
        );
    }

    #[test]
    fn a_data_offset_the_buffer_does_not_hold_is_refused() {
        let mut bytes = SYN;
        bytes[12] = 6 << 4;

        assert_eq!(
            Header::parse(&bytes),
            Err(Error::TooShort {
                needed: 24,
                found: MIN_HEADER_SIZE,
            })
        );
    }

    #[test]
    fn a_write_into_a_short_buffer_is_refused() {
        let mut bytes = [0u8; MIN_HEADER_SIZE - 1];

        assert_eq!(
            syn().write(&mut bytes),
            Err(Error::TooShort {
                needed: MIN_HEADER_SIZE,
                found: MIN_HEADER_SIZE - 1,
            })
        );
    }

    #[test]
    fn a_data_offset_beyond_the_four_bits_is_refused() {
        let mut header = syn();
        header.data_offset = 16;

        assert_eq!(
            header.write(&mut [0u8; MAX_HEADER_SIZE]),
            Err(Error::FieldTooLarge {
                field: "the TCP data offset",
                value: 16,
                max: 15,
            })
        );
    }

    #[test]
    fn a_flag_outside_the_six_bits_is_refused() {
        let mut header = syn();
        header.flags = Flags(0x40);

        assert_eq!(
            header.write(&mut [0u8; MAX_HEADER_SIZE]),
            Err(Error::FieldTooLarge {
                field: "the TCP flags",
                value: 0x40,
                max: 0x3f,
            })
        );
    }

    #[test]
    fn the_option_bytes_of_a_written_header_are_zero() {
        let mut header = syn();
        header.data_offset = 6;
        let mut bytes = [0xaa; MAX_HEADER_SIZE];

        header.write(&mut bytes).expect("the buffer holds a header");

        assert_eq!(&bytes[MIN_HEADER_SIZE..24], &[0, 0, 0, 0]);
    }

    #[test]
    fn the_protocol_number_is_the_one_of_ipv4() {
        assert_eq!(PROTOCOL, ipv4::PROTOCOL_TCP);
    }
}
