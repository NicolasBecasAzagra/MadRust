// SPDX-License-Identifier: GPL-2.0-only
//! The frame header of an Ethernet link.
//!
//! The header of a frame is the hardware address of its destination, the
//! hardware address of its source and the type of the payload. The type tells
//! the receiver what the payload is: [`TYPE_ARP`] for an address resolution
//! packet of [`crate::arp`], [`TYPE_IPV4`] for an IPv4 datagram of
//! [`crate::ipv4`].
//!
//! A frame shorter than [`MIN_FRAME_SIZE`] is a runt, and a switch drops it.
//! [`build`] therefore pads a short payload with zeros and returns the padded
//! length, while [`frame_length`] says that length before anything is written.

use crate::Error;

/// Size of the header of a frame: two addresses and a type.
pub const HEADER_SIZE: usize = 14;

/// Size of one hardware address of this link.
pub const ADDRESS_SIZE: usize = 6;

/// Smallest frame a link carries without the check sequence.
pub const MIN_FRAME_SIZE: usize = 60;

/// Destination address of a frame every station of the link receives.
pub const BROADCAST: [u8; ADDRESS_SIZE] = [0xff; ADDRESS_SIZE];

/// Type of a payload that carries an IPv4 datagram.
pub const TYPE_IPV4: u16 = 0x0800;

/// Type of a payload that carries an address resolution packet.
pub const TYPE_ARP: u16 = 0x0806;

/// The three fields of a frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Hardware address of the station the frame is for.
    pub destination: [u8; ADDRESS_SIZE],
    /// Hardware address of the station that sends the frame.
    pub source: [u8; ADDRESS_SIZE],
    /// Type of the payload, for example [`TYPE_ARP`].
    pub ether_type: u16,
}

impl Header {
    /// Assemble a header from its three fields.
    pub const fn new(
        destination: [u8; ADDRESS_SIZE],
        source: [u8; ADDRESS_SIZE],
        ether_type: u16,
    ) -> Self {
        Self {
            destination,
            source,
            ether_type,
        }
    }

    /// Take a header out of the first [`HEADER_SIZE`] bytes.
    ///
    /// The buffer must hold the whole header, so a short frame is refused
    /// before a byte is read.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < HEADER_SIZE {
            return Err(Error::TooShort {
                needed: HEADER_SIZE,
                found: bytes.len(),
            });
        }

        let mut destination = [0u8; ADDRESS_SIZE];
        destination.copy_from_slice(&bytes[0..ADDRESS_SIZE]);
        let mut source = [0u8; ADDRESS_SIZE];
        source.copy_from_slice(&bytes[ADDRESS_SIZE..2 * ADDRESS_SIZE]);

        Ok(Self {
            destination,
            source,
            ether_type: u16::from_be_bytes([bytes[12], bytes[13]]),
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

        into[0..ADDRESS_SIZE].copy_from_slice(&self.destination);
        into[ADDRESS_SIZE..2 * ADDRESS_SIZE].copy_from_slice(&self.source);
        into[12..14].copy_from_slice(&self.ether_type.to_be_bytes());
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

/// One header and the payload that follows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    /// The three fields of the frame header.
    pub header: Header,
    /// The bytes after the header, padding included.
    pub payload: &'a [u8],
}

/// Length a frame has on the wire when it carries `payload_length` bytes.
///
/// A short payload is padded to [`MIN_FRAME_SIZE`], which is why the length is
/// larger than the header plus the payload.
pub fn frame_length(payload_length: usize) -> Result<usize, Error> {
    let length = HEADER_SIZE
        .checked_add(payload_length)
        .ok_or(Error::TooLong(payload_length))?;
    Ok(if length < MIN_FRAME_SIZE {
        MIN_FRAME_SIZE
    } else {
        length
    })
}

/// Build a frame of `header` and `payload` in the first bytes of `frame`.
///
/// The padding after a short payload is zeroed, so the same frame is produced
/// every time. Returns the length of the frame, padding included.
pub fn build(header: &Header, payload: &[u8], frame: &mut [u8]) -> Result<usize, Error> {
    let length = frame_length(payload.len())?;
    if frame.len() < length {
        return Err(Error::TooShort {
            needed: length,
            found: frame.len(),
        });
    }

    header.write(frame)?;
    let end = HEADER_SIZE + payload.len();
    frame[HEADER_SIZE..end].copy_from_slice(payload);
    frame[end..length].fill(0);
    Ok(length)
}

/// Take a frame apart into its header and its payload.
///
/// The payload keeps the padding of a short frame: the type of the payload
/// says how much of it means something.
pub fn parse(frame: &[u8]) -> Result<Frame<'_>, Error> {
    let header = Header::parse(frame)?;
    Ok(Frame {
        header,
        payload: &frame[HEADER_SIZE..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: [u8; MIN_FRAME_SIZE] = [
        0x52, 0x55, 0x0a, 0x00, 0x02, 0x02, // destination
        0x52, 0x54, 0x00, 0x12, 0x34, 0x56, // source
        0x08, 0x00, // type
        // payload, then padding
        0x45, 0x00, 0x00, 0x1c, 0x00, 0x01, 0x00, 0x00, 0x40, 0x11, 0x00, 0x00, 0x0a, 0x00, 0x02,
        0x0f, 0x0a, 0x00, 0x02, 0x02, 0x30, 0x39, 0x00, 0x35, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];

    #[test]
    fn a_frame_of_sixty_bytes_is_taken_apart() {
        let frame = parse(&FRAME).expect("the frame is sixty bytes");

        assert_eq!(
            frame.header.destination,
            [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02]
        );
        assert_eq!(frame.header.source, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        assert_eq!(frame.header.ether_type, TYPE_IPV4);
        assert_eq!(frame.payload.len(), MIN_FRAME_SIZE - HEADER_SIZE);
        assert_eq!(frame.payload[0], 0x45);
    }

    #[test]
    fn a_header_survives_the_round_trip() {
        let header = Header::new(BROADCAST, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56], TYPE_ARP);

        assert_eq!(header.to_bytes(), FRAME_HEADER);
        assert_eq!(Header::parse(&FRAME_HEADER), Ok(header));
    }

    const FRAME_HEADER: [u8; HEADER_SIZE] = [
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x52, 0x54, 0x00, 0x12, 0x34, 0x56, 0x08, 0x06,
    ];

    #[test]
    fn a_short_payload_gives_a_frame_of_sixty_bytes() {
        assert_eq!(frame_length(0), Ok(MIN_FRAME_SIZE));
        assert_eq!(frame_length(28), Ok(MIN_FRAME_SIZE));
        assert_eq!(frame_length(46), Ok(MIN_FRAME_SIZE));
        assert_eq!(frame_length(47), Ok(MIN_FRAME_SIZE + 1));
    }

    #[test]
    fn build_pads_the_payload_with_zeros() {
        let header = Header::new(BROADCAST, [1; ADDRESS_SIZE], TYPE_ARP);
        let mut frame = [0xaa; MIN_FRAME_SIZE + 4];

        let length = build(&header, &[0x11; 4], &mut frame).expect("the frame is large enough");

        assert_eq!(length, MIN_FRAME_SIZE);
        assert_eq!(&frame[..HEADER_SIZE], &header.to_bytes());
        assert_eq!(&frame[HEADER_SIZE..HEADER_SIZE + 4], &[0x11; 4]);
        assert_eq!(&frame[HEADER_SIZE + 4..MIN_FRAME_SIZE], &[0; 42]);
        assert_eq!(&frame[MIN_FRAME_SIZE..], &[0xaa; 4]);
    }

    #[test]
    fn a_frame_shorter_than_the_header_is_refused() {
        let frame = [0u8; HEADER_SIZE - 1];

        assert_eq!(
            parse(&frame),
            Err(Error::TooShort {
                needed: HEADER_SIZE,
                found: HEADER_SIZE - 1,
            })
        );
    }

    #[test]
    fn a_buffer_smaller_than_the_frame_is_refused() {
        let header = Header::new(BROADCAST, [1; ADDRESS_SIZE], TYPE_ARP);
        let mut frame = [0u8; MIN_FRAME_SIZE - 1];

        assert_eq!(
            build(&header, &[], &mut frame),
            Err(Error::TooShort {
                needed: MIN_FRAME_SIZE,
                found: MIN_FRAME_SIZE - 1,
            })
        );
    }
}
