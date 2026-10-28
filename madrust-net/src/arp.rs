// SPDX-License-Identifier: GPL-2.0-only
//! Address resolution over IPv4 on an Ethernet link (RFC 826).
//!
//! A station that knows the protocol address of its peer but not its hardware
//! address sends a request with the hardware address of the target left at
//! zero. The peer answers with the same fields, the two address pairs swapped
//! and the operation of a reply, so the sender learns the hardware address it
//! asked about.
//!
//! The packet is fixed: [`PACKET_SIZE`] bytes of fields whose sizes are the two
//! address lengths it carries. This crate only knows the lengths of this link,
//! six bytes of hardware address and four of protocol address, and refuses any
//! other pair instead of guessing.

use crate::{ethernet, Error};

/// Hardware type of an Ethernet link.
pub const HARDWARE_ETHERNET: u16 = 1;

/// Protocol type of an IPv4 address.
pub const PROTOCOL_IPV4: u16 = 0x0800;

/// Operation of a request: the sender asks for the hardware address of the
/// target.
pub const OPERATION_REQUEST: u16 = 1;

/// Operation of a reply: the sender answers with its own hardware address.
pub const OPERATION_REPLY: u16 = 2;

/// Length of a hardware address on an Ethernet link.
pub const HARDWARE_ADDRESS_LENGTH: u8 = 6;

/// Length of an IPv4 address.
pub const PROTOCOL_ADDRESS_LENGTH: u8 = 4;

/// Size of the payload of an address resolution packet on this link.
pub const PACKET_SIZE: usize = 28;

/// Offset of the sender hardware address inside the payload.
const SENDER_HARDWARE: usize = 8;

/// Offset of the sender protocol address inside the payload.
const SENDER_PROTOCOL: usize = 14;

/// Offset of the target hardware address inside the payload.
const TARGET_HARDWARE: usize = 18;

/// Offset of the target protocol address inside the payload.
const TARGET_PROTOCOL: usize = 24;

/// The ten fields of an address resolution packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet {
    /// Type of the hardware of the link, [`HARDWARE_ETHERNET`] here.
    pub hardware_type: u16,
    /// Type of the protocol address, [`PROTOCOL_IPV4`] here.
    pub protocol_type: u16,
    /// Length of a hardware address, [`HARDWARE_ADDRESS_LENGTH`] here.
    pub hardware_length: u8,
    /// Length of a protocol address, [`PROTOCOL_ADDRESS_LENGTH`] here.
    pub protocol_length: u8,
    /// [`OPERATION_REQUEST`] or [`OPERATION_REPLY`].
    pub operation: u16,
    /// Hardware address of the sender.
    pub sender_hardware: [u8; HARDWARE_ADDRESS_LENGTH as usize],
    /// Protocol address of the sender.
    pub sender_protocol: [u8; PROTOCOL_ADDRESS_LENGTH as usize],
    /// Hardware address of the target, zero in a request.
    pub target_hardware: [u8; HARDWARE_ADDRESS_LENGTH as usize],
    /// Protocol address of the target.
    pub target_protocol: [u8; PROTOCOL_ADDRESS_LENGTH as usize],
}

impl Packet {
    /// A request that asks for the hardware address of `target_protocol`.
    ///
    /// The hardware address of the target stays zero: that is what the request
    /// asks the peer to fill in.
    pub const fn request(
        sender_hardware: [u8; HARDWARE_ADDRESS_LENGTH as usize],
        sender_protocol: [u8; PROTOCOL_ADDRESS_LENGTH as usize],
        target_protocol: [u8; PROTOCOL_ADDRESS_LENGTH as usize],
    ) -> Self {
        Self {
            hardware_type: HARDWARE_ETHERNET,
            protocol_type: PROTOCOL_IPV4,
            hardware_length: HARDWARE_ADDRESS_LENGTH,
            protocol_length: PROTOCOL_ADDRESS_LENGTH,
            operation: OPERATION_REQUEST,
            sender_hardware,
            sender_protocol,
            target_hardware: [0; HARDWARE_ADDRESS_LENGTH as usize],
            target_protocol,
        }
    }

    /// Take a packet out of the first [`PACKET_SIZE`] bytes.
    ///
    /// A packet with other address lengths is refused: the size of its fields
    /// would not be the one of this structure, so the bytes cannot be read as
    /// four addresses of this link.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < PACKET_SIZE {
            return Err(Error::TooShort {
                needed: PACKET_SIZE,
                found: bytes.len(),
            });
        }

        let hardware_length = bytes[4];
        let protocol_length = bytes[5];
        if hardware_length != HARDWARE_ADDRESS_LENGTH || protocol_length != PROTOCOL_ADDRESS_LENGTH
        {
            return Err(Error::UnsupportedAddressLengths {
                hardware: hardware_length,
                protocol: protocol_length,
            });
        }

        let mut sender_hardware = [0u8; HARDWARE_ADDRESS_LENGTH as usize];
        sender_hardware.copy_from_slice(&bytes[SENDER_HARDWARE..SENDER_HARDWARE + 6]);
        let mut sender_protocol = [0u8; PROTOCOL_ADDRESS_LENGTH as usize];
        sender_protocol.copy_from_slice(&bytes[SENDER_PROTOCOL..SENDER_PROTOCOL + 4]);
        let mut target_hardware = [0u8; HARDWARE_ADDRESS_LENGTH as usize];
        target_hardware.copy_from_slice(&bytes[TARGET_HARDWARE..TARGET_HARDWARE + 6]);
        let mut target_protocol = [0u8; PROTOCOL_ADDRESS_LENGTH as usize];
        target_protocol.copy_from_slice(&bytes[TARGET_PROTOCOL..TARGET_PROTOCOL + 4]);

        Ok(Self {
            hardware_type: u16::from_be_bytes([bytes[0], bytes[1]]),
            protocol_type: u16::from_be_bytes([bytes[2], bytes[3]]),
            hardware_length,
            protocol_length,
            operation: u16::from_be_bytes([bytes[6], bytes[7]]),
            sender_hardware,
            sender_protocol,
            target_hardware,
            target_protocol,
        })
    }

    /// Write the packet into the first [`PACKET_SIZE`] bytes of `into`.
    pub fn write(&self, into: &mut [u8]) -> Result<(), Error> {
        if into.len() < PACKET_SIZE {
            return Err(Error::TooShort {
                needed: PACKET_SIZE,
                found: into.len(),
            });
        }

        into[0..2].copy_from_slice(&self.hardware_type.to_be_bytes());
        into[2..4].copy_from_slice(&self.protocol_type.to_be_bytes());
        into[4] = self.hardware_length;
        into[5] = self.protocol_length;
        into[6..8].copy_from_slice(&self.operation.to_be_bytes());
        into[SENDER_HARDWARE..SENDER_HARDWARE + 6].copy_from_slice(&self.sender_hardware);
        into[SENDER_PROTOCOL..SENDER_PROTOCOL + 4].copy_from_slice(&self.sender_protocol);
        into[TARGET_HARDWARE..TARGET_HARDWARE + 6].copy_from_slice(&self.target_hardware);
        into[TARGET_PROTOCOL..TARGET_PROTOCOL + 4].copy_from_slice(&self.target_protocol);
        Ok(())
    }

    /// The packet as the bytes of the wire format.
    pub fn to_bytes(&self) -> [u8; PACKET_SIZE] {
        let mut bytes = [0u8; PACKET_SIZE];
        // The array has exactly the size `write` needs, so this cannot fail.
        let _ = self.write(&mut bytes);
        bytes
    }
}

/// Build the frame of a request, pad it to [`ethernet::MIN_FRAME_SIZE`] and
/// return it.
///
/// The destination of the frame is the broadcast address, because the hardware
/// address of the target is what the request asks for.
pub fn request_frame(
    source: [u8; ethernet::ADDRESS_SIZE],
    sender_protocol: [u8; PROTOCOL_ADDRESS_LENGTH as usize],
    target_protocol: [u8; PROTOCOL_ADDRESS_LENGTH as usize],
) -> Result<[u8; ethernet::MIN_FRAME_SIZE], Error> {
    let header = ethernet::Header::new(ethernet::BROADCAST, source, ethernet::TYPE_ARP);
    let packet = Packet::request(source, sender_protocol, target_protocol);
    let mut frame = [0u8; ethernet::MIN_FRAME_SIZE];
    ethernet::build(&header, &packet.to_bytes(), &mut frame)?;
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST_HARDWARE: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const GATEWAY_HARDWARE: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];
    const GUEST_ADDRESS: [u8; 4] = [10, 0, 2, 15];
    const GATEWAY_ADDRESS: [u8; 4] = [10, 0, 2, 2];

    /// The request the kernel sent before this crate existed, byte for byte:
    /// broadcast destination, the hardware address of the device as the
    /// source, type `0x0806`, the request of the gateway, and zero padding.
    const REQUEST_FRAME: [u8; ethernet::MIN_FRAME_SIZE] = [
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // destination: broadcast
        0x52, 0x54, 0x00, 0x12, 0x34, 0x56, // source
        0x08, 0x06, // type: address resolution
        0x00, 0x01, // hardware type: Ethernet
        0x08, 0x00, // protocol type: IPv4
        0x06, // hardware address length
        0x04, // protocol address length
        0x00, 0x01, // operation: request
        0x52, 0x54, 0x00, 0x12, 0x34, 0x56, // sender hardware
        0x0a, 0x00, 0x02, 0x0f, // sender protocol: 10.0.2.15
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // target hardware: unknown
        0x0a, 0x00, 0x02, 0x02, // target protocol: 10.0.2.2
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // padding
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    /// The answer of the gateway, as it came back from the network: the same
    /// fields, the two address pairs swapped, operation two, and eighteen bytes
    /// of padding because the packet is shorter than the smallest frame.
    const REPLY_FRAME: [u8; ethernet::MIN_FRAME_SIZE] = [
        0x52, 0x54, 0x00, 0x12, 0x34, 0x56, // destination: the guest
        0x52, 0x55, 0x0a, 0x00, 0x02, 0x02, // source: the gateway
        0x08, 0x06, // type: address resolution
        0x00, 0x01, // hardware type: Ethernet
        0x08, 0x00, // protocol type: IPv4
        0x06, // hardware address length
        0x04, // protocol address length
        0x00, 0x02, // operation: reply
        0x52, 0x55, 0x0a, 0x00, 0x02, 0x02, // sender hardware: the gateway
        0x0a, 0x00, 0x02, 0x02, // sender protocol: 10.0.2.2
        0x52, 0x54, 0x00, 0x12, 0x34, 0x56, // target hardware: the guest
        0x0a, 0x00, 0x02, 0x0f, // target protocol: 10.0.2.15
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // padding
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn the_request_is_the_frame_the_kernel_sent() {
        let frame = request_frame(GUEST_HARDWARE, GUEST_ADDRESS, GATEWAY_ADDRESS)
            .expect("a request fits a frame of sixty bytes");

        assert_eq!(frame, REQUEST_FRAME);
    }

    #[test]
    fn the_request_keeps_the_target_hardware_address_at_zero() {
        let frame = request_frame(GUEST_HARDWARE, GUEST_ADDRESS, GATEWAY_ADDRESS)
            .expect("a request fits a frame of sixty bytes");
        let ethernet::Frame { header, payload } =
            ethernet::parse(&frame).expect("the frame is sixty bytes");

        assert_eq!(header.ether_type, ethernet::TYPE_ARP);
        let packet = Packet::parse(payload).expect("the payload is an address resolution packet");
        assert_eq!(packet.operation, OPERATION_REQUEST);
        assert_eq!(packet.hardware_type, HARDWARE_ETHERNET);
        assert_eq!(packet.protocol_type, PROTOCOL_IPV4);
        assert_eq!(packet.hardware_length, HARDWARE_ADDRESS_LENGTH);
        assert_eq!(packet.protocol_length, PROTOCOL_ADDRESS_LENGTH);
        assert_eq!(packet.sender_hardware, GUEST_HARDWARE);
        assert_eq!(packet.sender_protocol, GUEST_ADDRESS);
        assert_eq!(packet.target_hardware, [0; 6]);
        assert_eq!(packet.target_protocol, GATEWAY_ADDRESS);
    }

    #[test]
    fn the_reply_of_the_gateway_is_taken_apart() {
        let ethernet::Frame { header, payload } =
            ethernet::parse(&REPLY_FRAME).expect("the frame is sixty bytes");

        assert_eq!(header.ether_type, ethernet::TYPE_ARP);
        let packet = Packet::parse(payload).expect("the payload is an address resolution packet");
        assert_eq!(packet.operation, OPERATION_REPLY);
        assert_eq!(packet.sender_hardware, GATEWAY_HARDWARE);
        assert_eq!(packet.sender_protocol, GATEWAY_ADDRESS);
        assert_eq!(packet.target_hardware, GUEST_HARDWARE);
        assert_eq!(packet.target_protocol, GUEST_ADDRESS);
    }

    #[test]
    fn a_packet_survives_the_round_trip() {
        let packet = Packet::parse(REQUEST_FRAME[ethernet::HEADER_SIZE..].as_ref())
            .expect("the payload is an address resolution packet");

        assert_eq!(packet.to_bytes(), REQUEST_FRAME[14..42]);
        assert_eq!(Packet::parse(&packet.to_bytes()), Ok(packet));
    }

    #[test]
    fn a_packet_shorter_than_the_fields_is_refused() {
        assert_eq!(
            Packet::parse(&[0u8; PACKET_SIZE - 1]),
            Err(Error::TooShort {
                needed: PACKET_SIZE,
                found: PACKET_SIZE - 1,
            })
        );
    }

    #[test]
    fn other_address_lengths_are_refused() {
        let mut bytes = Packet::request(GUEST_HARDWARE, GUEST_ADDRESS, GATEWAY_ADDRESS).to_bytes();
        bytes[4] = 8;

        assert_eq!(
            Packet::parse(&bytes),
            Err(Error::UnsupportedAddressLengths {
                hardware: 8,
                protocol: PROTOCOL_ADDRESS_LENGTH,
            })
        );
    }
}
