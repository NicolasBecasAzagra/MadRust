// SPDX-License-Identifier: GPL-2.0-only
//! The path from the guest to the host over the virtio network card.
//!
//! The emulator user network gives the guest the address [`GUEST_ADDRESS`]
//! with the mask [`NETMASK`], and it answers as the gateway [`GATEWAY_ADDRESS`]
//! and as the name server [`NAME_SERVER`]. These addresses are fixed: there is
//! no firmware, no configuration protocol and no name server of this kernel
//! yet, so the module knows them.
//!
//! The path has three steps, in the order of the protocols: [`arp`] finds the
//! hardware address of the gateway, [`ipv4`] and [`udp`] carry the datagram,
//! and [`ethernet`] frames it. The frames themselves are built and read by the
//! crate `madrust_net`; this module only decides what to send and where the
//! answers go.
//!
//! Nothing is allocated: the transmit buffer belongs to the driver, and the
//! frames that arrive are read where the device wrote them, in the physical
//! page the driver posted. The driver is polled, because the interrupt of the
//! card is not routed.

use madrust_net::{arp, ethernet, ipv4, udp};

use crate::virtio::{Device, Error as DeviceError};

/// Address the emulator gives to the guest.
pub const GUEST_ADDRESS: [u8; 4] = [10, 0, 2, 15];

/// Mask of the user network of the emulator, twenty four bits.
pub const NETMASK: [u8; 4] = [255, 255, 255, 0];

/// Address of the gateway, and the address the host answers at.
pub const GATEWAY_ADDRESS: [u8; 4] = [10, 0, 2, 2];

/// Address of the name server of the emulator. The kernel does not speak to it
/// yet; the name of a host is not resolved in this milestone.
pub const NAME_SERVER: [u8; 4] = [10, 0, 2, 3];

/// Address the datagram goes to: the gateway, which is the host on this link.
pub const SERVER_ADDRESS: [u8; 4] = GATEWAY_ADDRESS;

/// Port of the guest. Any number above the reserved range works, because no
/// one else of the guest listens.
pub const GUEST_PORT: u16 = 49152;

/// Port of the server on the host, the one the gate of the pipeline binds.
pub const SERVER_PORT: u16 = 9999;

/// Bytes the guest sends to the server.
pub const MESSAGE: &[u8] = b"ping";

/// How many times a wait polls the card before it gives up.
///
/// The host answers in the same turn as the transmit, so the loop is short in
/// practice. The number is large because the kernel runs both with and without
/// the hardware virtualization of the processor, and a silent host has to end
/// in a reported failure instead of a boot that never finishes.
const POLL_LIMIT: u32 = 200_000_000;

/// Time to live of the datagrams the guest sends, the value the examples of
/// the protocol use.
const TIME_TO_LIVE: u8 = 64;

/// Flag of an IPv4 header that asks the routers not to fragment a datagram.
const FLAG_DO_NOT_FRAGMENT: u8 = 0b010;

/// Why the path failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The driver refused the frame or one of its queues stopped answering.
    Device(DeviceError),
    /// A protocol refused a frame the kernel built.
    Refused(madrust_net::Error),
    /// The gateway did not answer the address resolution request.
    GatewayTimeout,
    /// The server did not answer the datagram.
    AnswerTimeout,
    /// The gateway is not resolved, so no frame can be addressed to it.
    GatewayUnknown,
}

impl From<DeviceError> for Error {
    fn from(error: DeviceError) -> Self {
        Error::Device(error)
    }
}

/// Resolve the gateway, send one datagram to the server and wait for the
/// answer.
pub fn run(device: Device) -> Result<(), Error> {
    crate::kprintln!(
        "[net] guest={} mask={} gateway={} dns={} port={}",
        Address(GUEST_ADDRESS),
        Address(NETMASK),
        Address(GATEWAY_ADDRESS),
        Address(NAME_SERVER),
        GUEST_PORT
    );

    let mut interface = Interface::new(device);
    let hardware = interface.resolve(GATEWAY_ADDRESS)?;
    crate::kprintln!(
        "[net] arp gateway={} mac={}",
        Address(GATEWAY_ADDRESS),
        Hardware(hardware)
    );

    interface.exchange()
}

/// The card and what the kernel learned about its link.
struct Interface {
    /// The network device, polled for frames.
    device: Device,
    /// Hardware address of the gateway, once the answer arrived.
    gateway: Option<[u8; ethernet::ADDRESS_SIZE]>,
    /// Identification of the next IPv4 datagram.
    identification: u16,
}

impl Interface {
    /// An interface that knows no hardware address yet.
    fn new(device: Device) -> Self {
        Self {
            device,
            gateway: None,
            identification: 0,
        }
    }

    /// Learn the hardware address of `address`.
    ///
    /// The frames that are already waiting are read before the request goes
    /// out: the answer may have arrived before the question, and a frame left
    /// in the queue would make the wait below time out with the answer in
    /// hand.
    fn resolve(&mut self, address: [u8; 4]) -> Result<[u8; ethernet::ADDRESS_SIZE], Error> {
        self.poll(ignore)?;
        if let Some(hardware) = self.gateway {
            return Ok(hardware);
        }

        let request = arp::request_frame(self.device.mac(), GUEST_ADDRESS, address)
            .map_err(Error::Refused)?;
        self.device.transmit(|area| {
            if area.len() < request.len() {
                return None;
            }
            area[..request.len()].copy_from_slice(&request);
            Some(request.len())
        })?;

        let mut spins = 0;
        loop {
            self.poll(ignore)?;
            if let Some(hardware) = self.gateway {
                return Ok(hardware);
            }
            if spins == POLL_LIMIT {
                return Err(Error::GatewayTimeout);
            }
            spins += 1;
            core::hint::spin_loop();
        }
    }

    /// Send one datagram to the server and wait for the answer to the guest
    /// port.
    fn exchange(&mut self) -> Result<(), Error> {
        let gateway = self.gateway.ok_or(Error::GatewayUnknown)?;
        let hardware = self.device.mac();
        let identification = self.identification;
        self.identification = self.identification.wrapping_add(1);

        self.device
            .transmit(|area| build(area, hardware, gateway, identification, MESSAGE))?;
        crate::kprintln!(
            "[net] udp send {}:{} -> {}:{} len={}",
            Address(GUEST_ADDRESS),
            GUEST_PORT,
            Address(SERVER_ADDRESS),
            SERVER_PORT,
            MESSAGE.len()
        );

        let mut answered = false;
        let mut spins = 0;
        while !answered {
            self.poll(
                |source, source_port, destination, destination_port, payload| {
                    crate::kprintln!(
                        "[net] udp recv {}:{} -> {}:{} len={} data=\"{}\"",
                        Address(source),
                        source_port,
                        Address(destination),
                        destination_port,
                        payload.len(),
                        core::str::from_utf8(payload).unwrap_or("<binary>")
                    );
                    answered = true;
                },
            )?;
            if answered {
                return Ok(());
            }
            if spins == POLL_LIMIT {
                return Err(Error::AnswerTimeout);
            }
            spins += 1;
            core::hint::spin_loop();
        }
        Ok(())
    }

    /// Read the frames that arrived and hand the datagrams of [`GUEST_PORT`]
    /// to `deliver`.
    ///
    /// Returns the number of frames that were read.
    fn poll(
        &mut self,
        mut deliver: impl FnMut([u8; 4], u16, [u8; 4], u16, &[u8]),
    ) -> Result<usize, Error> {
        let mut gateway = self.gateway;
        let frames = self
            .device
            .poll(|frame| inspect(frame, &mut gateway, &mut deliver))?;
        self.gateway = gateway;
        Ok(frames)
    }
}

/// Take one frame apart and send what it carries to the right place.
///
/// Every frame the kernel has no use for is dropped with the reason, because a
/// frame that disappears without a word cannot be told apart from one that
/// never arrived.
fn inspect(
    frame: &[u8],
    gateway: &mut Option<[u8; ethernet::ADDRESS_SIZE]>,
    deliver: &mut impl FnMut([u8; 4], u16, [u8; 4], u16, &[u8]),
) {
    let frame = match ethernet::parse(frame) {
        Ok(frame) => frame,
        Err(reason) => {
            crate::kprintln!("[net] drop: {}", reason);
            return;
        }
    };

    match frame.header.ether_type {
        ethernet::TYPE_ARP => {
            if !learn(frame.payload, gateway) {
                crate::kprintln!("[net] drop type=0x0806: not a reply of the gateway");
            }
        }
        ethernet::TYPE_IPV4 => receive(frame.payload, deliver),
        other => crate::kprintln!("[net] drop type=0x{:04x}: not an IPv4 frame", other),
    }
}

/// Learn the hardware address of the gateway from an address resolution reply.
///
/// Returns `false` when the frame is an address resolution packet that does not
/// name the gateway, so the caller can report it. A request of another station
/// is not a failure: the emulator sends one when it takes the link up. A packet
/// that cannot be read is reported here and counts as taken.
fn learn(payload: &[u8], gateway: &mut Option<[u8; ethernet::ADDRESS_SIZE]>) -> bool {
    let packet = match arp::Packet::parse(payload) {
        Ok(packet) => packet,
        Err(reason) => {
            crate::kprintln!("[net] drop: {}", reason);
            return true;
        }
    };

    if packet.operation != arp::OPERATION_REPLY
        || packet.sender_protocol != GATEWAY_ADDRESS
        || packet.target_protocol != GUEST_ADDRESS
    {
        return false;
    }

    *gateway = Some(packet.sender_hardware);
    true
}

/// Take an IPv4 datagram apart and deliver it when it is UDP for this guest.
fn receive(payload: &[u8], deliver: &mut impl FnMut([u8; 4], u16, [u8; 4], u16, &[u8])) {
    let packet = match ipv4::parse(payload) {
        Ok(packet) => packet,
        Err(reason) => {
            crate::kprintln!("[net] drop: {}", reason);
            return;
        }
    };

    if packet.header.protocol != ipv4::PROTOCOL_UDP {
        crate::kprintln!(
            "[net] drop protocol={}: not a UDP datagram",
            packet.header.protocol
        );
        return;
    }

    let datagram = match udp::parse(packet.payload) {
        Ok(datagram) => datagram,
        Err(reason) => {
            crate::kprintln!("[net] drop: {}", reason);
            return;
        }
    };

    if let Err(reason) = udp::verify(
        packet.header.source,
        packet.header.destination,
        packet.payload,
    ) {
        crate::kprintln!("[net] drop: {}", reason);
        return;
    }

    if datagram.header.destination_port != GUEST_PORT {
        crate::kprintln!(
            "[net] drop port={}: no one asked for it",
            datagram.header.destination_port
        );
        return;
    }

    deliver(
        packet.header.source,
        datagram.header.source_port,
        packet.header.destination,
        datagram.header.destination_port,
        datagram.payload,
    );
}

/// Write one datagram into `area` and return the length of the whole frame.
///
/// The frame goes to the gateway, because the server sits behind it, so its
/// hardware address is the destination of the frame while the address of the
/// datagram stays the one of the server. Every header is computed and written
/// by the crate; the padding that brings the frame to the smallest length of
/// the link is zeroed. Returns `None` when a length does not fit its field or
/// the frame does not fit the buffer of the device.
fn build(
    area: &mut [u8],
    source_hardware: [u8; ethernet::ADDRESS_SIZE],
    gateway_hardware: [u8; ethernet::ADDRESS_SIZE],
    identification: u16,
    payload: &[u8],
) -> Option<usize> {
    let udp_length = udp::HEADER_SIZE.checked_add(payload.len())?;
    let udp_length = u16::try_from(udp_length).ok()?;
    let total_length = ipv4::MIN_HEADER_SIZE.checked_add(usize::from(udp_length))?;
    let total_length = u16::try_from(total_length).ok()?;
    let frame_length = ethernet::frame_length(usize::from(total_length)).ok()?;
    if area.len() < frame_length {
        return None;
    }

    let header = ethernet::Header::new(gateway_hardware, source_hardware, ethernet::TYPE_IPV4);
    header.write(area).ok()?;

    let mut header = ipv4::Header {
        version: ipv4::VERSION,
        header_length: (ipv4::MIN_HEADER_SIZE / 4) as u8,
        type_of_service: 0,
        total_length,
        identification,
        flags: FLAG_DO_NOT_FRAGMENT,
        fragment_offset: 0,
        time_to_live: TIME_TO_LIVE,
        protocol: ipv4::PROTOCOL_UDP,
        checksum: 0,
        source: GUEST_ADDRESS,
        destination: SERVER_ADDRESS,
    };
    header.fill_checksum().ok()?;
    let ip_start = ethernet::HEADER_SIZE;
    header.write(&mut area[ip_start..]).ok()?;

    // The checksum of UDP covers the header with its checksum field at zero and
    // the payload, so the datagram is written first and the field is filled
    // once the bytes are in place.
    let udp_start = ip_start + ipv4::MIN_HEADER_SIZE;
    let datagram = &mut area[udp_start..udp_start + usize::from(udp_length)];
    let mut header = udp::Header {
        source_port: GUEST_PORT,
        destination_port: SERVER_PORT,
        length: udp_length,
        checksum: 0,
    };
    header.write(datagram).ok()?;
    datagram[udp::HEADER_SIZE..].copy_from_slice(payload);
    header.checksum = udp::checksum(GUEST_ADDRESS, SERVER_ADDRESS, datagram).ok()?;
    header.write(datagram).ok()?;

    let end = ip_start + usize::from(total_length);
    area[end..frame_length].fill(0);
    Some(frame_length)
}

/// Do nothing with a datagram: the caller is not waiting for one.
fn ignore(_: [u8; 4], _: u16, _: [u8; 4], _: u16, _: &[u8]) {}

/// One IPv4 address, written the way the dotted notation of the protocol does.
struct Address([u8; 4]);

impl core::fmt::Display for Address {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "{}.{}.{}.{}",
            self.0[0], self.0[1], self.0[2], self.0[3]
        )
    }
}

/// One hardware address, written the way the link notation does.
struct Hardware([u8; ethernet::ADDRESS_SIZE]);

impl core::fmt::Display for Hardware {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}
