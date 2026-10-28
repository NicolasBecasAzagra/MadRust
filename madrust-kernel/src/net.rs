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
//! hardware address of a neighbour, [`ipv4`] and [`udp`] carry the datagram,
//! and [`ethernet`] frames it. The frames themselves are built and read by the
//! crate `madrust_net`; this module only decides what to send and where the
//! answers go.
//!
//! Nothing is allocated: the transmit buffer belongs to the driver, and the
//! frames that arrive are read where the device wrote them, in the physical
//! page the driver posted. The driver is polled, because the interrupt of the
//! card is not routed.
//!
//! ## Sockets
//!
//! [`run`] installs the interface in a table of datagram sockets after the
//! fixed demonstration exchange. The Linux socket calls of [`crate::user`]
//! reach that table through [`socket_open`], [`socket_bind`], [`socket_connect`], [`socket_send`],
//! [`socket_receive`], [`socket_local`], [`socket_readable`] and [`socket_writable`].
//! A socket keeps its local and remote endpoints and a small queue for the
//! datagrams that arrived before the program asked for them. The queue is
//! fixed, so no socket path allocates.
//!
//! A receive with an empty queue reports a would block instead of sleeping.
//! The descriptor table holds its lock with interrupts off and must not be
//! held while a thread waits, and the caller of this module already copies the
//! socket number out of that table before it gets here.

use core::cell::UnsafeCell;

use madrust_net::{arp, ethernet, ipv4, udp};

use crate::sync::{IrqLockGuard, IrqSpinLock};
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

/// Address the datagram of the demonstration goes to: the gateway, which is
/// the host on this link.
pub const SERVER_ADDRESS: [u8; 4] = GATEWAY_ADDRESS;

/// Port of the demonstration exchange. Any number above the reserved range
/// works, because no one else of the guest listens on it.
pub const GUEST_PORT: u16 = 49152;

/// Port of the server on the host, the one the gate of the pipeline binds.
pub const SERVER_PORT: u16 = 9999;

/// Bytes the guest sends to the server in the demonstration exchange.
pub const MESSAGE: &[u8] = b"ping";

/// Number of sockets the table can hold.
const MAX_SOCKETS: usize = 8;

/// Number of datagrams one socket can hold before it reports a would block.
const QUEUE_DEPTH: usize = 4;

/// Largest payload a datagram can carry on the link the emulator gives us.
const MAX_DATAGRAM: usize = 1472;

/// First port the kernel hands out when a socket binds port zero.
const EPHEMERAL_FIRST: u16 = 32768;

/// Last port of the range above.
const EPHEMERAL_LAST: u16 = 60999;

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
    /// The neighbour did not answer the address resolution request.
    GatewayTimeout,
    /// The server did not answer the datagram.
    AnswerTimeout,
}

impl From<DeviceError> for Error {
    fn from(error: DeviceError) -> Self {
        Error::Device(error)
    }
}

/// Why a socket call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketError {
    /// The descriptor does not name an open datagram socket.
    NotSocket,
    /// Another socket already owns the local port.
    AddressInUse,
    /// The local address is not one of this link.
    AddressNotAvailable,
    /// The destination is outside the subnet of the guest.
    NetworkUnreachable,
    /// The destination did not answer the address resolution request.
    HostUnreachable,
    /// The socket has no other end, so the datagram has nowhere to go.
    NotConnected,
    /// The socket already has an other end.
    IsConnected,
    /// A length or an endpoint the call cannot use.
    Invalid,
    /// The table has no free slot, or no port is free.
    NoBuffer,
    /// The machine has no network device.
    Unsupported,
}

/// One datagram that arrived and nobody read.
#[derive(Clone, Copy)]
struct Datagram {
    source_address: [u8; 4],
    source_port: u16,
    length: usize,
    data: [u8; MAX_DATAGRAM],
}

/// One datagram handed to the program by [`socket_receive`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Received {
    /// Address the datagram came from.
    pub source_address: [u8; 4],
    /// Port the datagram came from.
    pub source_port: u16,
    /// Number of bytes copied into the buffer of the program.
    pub length: usize,
}

/// State of one datagram socket.
#[derive(Clone, Copy)]
struct Socket {
    /// The slot is in use.
    open: bool,
    /// A local address and port were chosen.
    bound: bool,
    /// Local address, which may be the wildcard `0.0.0.0`.
    local_address: [u8; 4],
    /// Local port.
    local_port: u16,
    /// The other end was chosen by `connect`.
    connected: bool,
    /// Address of the other end.
    remote_address: [u8; 4],
    /// Port of the other end.
    remote_port: u16,
    /// Position of the oldest datagram of the queue.
    head: usize,
    /// Number of datagrams in the queue.
    queued: usize,
    /// Datagrams that arrived before the program asked for them.
    queue: [Option<Datagram>; QUEUE_DEPTH],
}

impl Socket {
    /// A slot that no descriptor refers to.
    const EMPTY: Self = Self {
        open: false,
        bound: false,
        local_address: [0; 4],
        local_port: 0,
        connected: false,
        remote_address: [0; 4],
        remote_port: 0,
        head: 0,
        queued: 0,
        queue: [None; QUEUE_DEPTH],
    };
}

/// The interface and the sockets that share it.
struct Network {
    interface: Option<Interface>,
    sockets: [Socket; MAX_SOCKETS],
}

impl Network {
    /// A table with no interface and no socket.
    const fn new() -> Self {
        Self {
            interface: None,
            sockets: [Socket::EMPTY; MAX_SOCKETS],
        }
    }
}

struct Cell(UnsafeCell<Network>);

// SAFETY: the whole table is only reached through `lock`, which holds an
// interrupt safe spinlock for the duration of one operation.
unsafe impl Sync for Cell {}

static NETWORK: Cell = Cell(UnsafeCell::new(Network::new()));

/// Spinlock over the interface and the socket table.
static LOCK: IrqSpinLock = IrqSpinLock::new("net");

/// Enter the critical section that guards [`NETWORK`].
fn lock() -> IrqLockGuard<'static> {
    LOCK.lock()
}

/// Borrow the network table. The caller must hold [`LOCK`].
fn table() -> &'static mut Network {
    // SAFETY: the caller holds `LOCK`, so no other path can borrow the table
    // until the guard is dropped.
    unsafe { &mut *NETWORK.0.get() }
}

/// Resolve the gateway, send one datagram to the server and wait for the
/// answer. Then keep the interface for the socket calls.
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
    let hardware = match interface.resolve(GATEWAY_ADDRESS) {
        Ok(hardware) => hardware,
        Err(error) => {
            // The link is still usable: a socket retries the resolution when
            // it sends its first datagram.
            install(interface);
            return Err(error);
        }
    };
    crate::kprintln!(
        "[net] arp gateway={} mac={}",
        Address(GATEWAY_ADDRESS),
        Hardware(hardware)
    );

    let exchange = interface.exchange();

    // The demonstration exchange may fail on a quiet host, but the interface
    // is still usable: the sockets retry the address resolution themselves.
    install(interface);
    exchange
}

/// Put the interface in the table the socket calls reach. A machine without a
/// card never reaches this point.
fn install(interface: Interface) {
    let _guard = lock();
    table().interface = Some(interface);
}

/// The card and what the kernel learned about its link.
struct Interface {
    /// The network device, polled for frames.
    device: Device,
    /// Address and hardware address of the last neighbour that answered.
    neighbour: Option<([u8; 4], [u8; ethernet::ADDRESS_SIZE])>,
    /// Identification of the next IPv4 datagram.
    identification: u16,
}

impl Interface {
    /// An interface that knows no hardware address yet.
    fn new(device: Device) -> Self {
        Self {
            device,
            neighbour: None,
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
        if let Some(hardware) = self.known(address) {
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
            if let Some(hardware) = self.known(address) {
                return Ok(hardware);
            }
            if spins == POLL_LIMIT {
                return Err(Error::GatewayTimeout);
            }
            spins += 1;
            core::hint::spin_loop();
        }
    }

    /// Hardware address of `address`, when the last answer named it.
    fn known(&self, address: [u8; 4]) -> Option<[u8; ethernet::ADDRESS_SIZE]> {
        match self.neighbour {
            Some((known, hardware)) if known == address => Some(hardware),
            _ => None,
        }
    }

    /// Send one datagram and wait for the answer to the demonstration port.
    fn exchange(&mut self) -> Result<(), Error> {
        self.send(
            GUEST_ADDRESS,
            GUEST_PORT,
            SERVER_ADDRESS,
            SERVER_PORT,
            MESSAGE,
        )?;
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
                    if source == SERVER_ADDRESS
                        && source_port == SERVER_PORT
                        && destination_port == GUEST_PORT
                    {
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
                    }
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

    /// Send one datagram from the link of the guest.
    fn send(
        &mut self,
        source_address: [u8; 4],
        source_port: u16,
        destination_address: [u8; 4],
        destination_port: u16,
        payload: &[u8],
    ) -> Result<(), Error> {
        let hardware = self.resolve(destination_address)?;
        let identification = self.identification;
        self.identification = self.identification.wrapping_add(1);
        let local_hardware = self.device.mac();

        self.device.transmit(|area| {
            build(
                area,
                local_hardware,
                hardware,
                identification,
                source_address,
                source_port,
                destination_address,
                destination_port,
                payload,
            )
        })?;
        Ok(())
    }

    /// Read the frames that arrived and hand the UDP datagrams to `deliver`.
    ///
    /// Returns the number of frames that were read.
    fn poll(
        &mut self,
        mut deliver: impl FnMut([u8; 4], u16, [u8; 4], u16, &[u8]),
    ) -> Result<usize, Error> {
        let mut neighbour = self.neighbour;
        let frames = self
            .device
            .poll(|frame| inspect(frame, &mut neighbour, &mut deliver))?;
        self.neighbour = neighbour;
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
    neighbour: &mut Option<([u8; 4], [u8; ethernet::ADDRESS_SIZE])>,
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
            if !learn(frame.payload, neighbour) {
                crate::kprintln!("[net] drop type=0x0806: not a reply for the guest");
            }
        }
        ethernet::TYPE_IPV4 => receive(frame.payload, deliver),
        other => crate::kprintln!("[net] drop type=0x{:04x}: not an IPv4 frame", other),
    }
}

/// Learn the hardware address of a neighbour from an address resolution reply.
///
/// Returns `false` when the frame is an address resolution packet that does not
/// answer the guest, so the caller can report it. A request of another station
/// is not a failure: the emulator sends one when it takes the link up. A packet
/// that cannot be read is reported here and counts as taken.
fn learn(payload: &[u8], neighbour: &mut Option<([u8; 4], [u8; ethernet::ADDRESS_SIZE])>) -> bool {
    let packet = match arp::Packet::parse(payload) {
        Ok(packet) => packet,
        Err(reason) => {
            crate::kprintln!("[net] drop: {}", reason);
            return true;
        }
    };

    if packet.operation != arp::OPERATION_REPLY || packet.target_protocol != GUEST_ADDRESS {
        return false;
    }

    *neighbour = Some((packet.sender_protocol, packet.sender_hardware));
    true
}

/// Take an IPv4 datagram apart and deliver it when it is a UDP datagram for
/// the guest.
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
/// The frame goes to the neighbour, so its hardware address is the destination
/// of the frame while the address of the datagram is the destination of the
/// call. Every header is computed and written by the crate; the padding that
/// brings the frame to the smallest length of the link is zeroed. Returns
/// `None` when a length does not fit its field or the frame does not fit the
/// buffer of the device.
#[allow(clippy::too_many_arguments)]
fn build(
    area: &mut [u8],
    source_hardware: [u8; ethernet::ADDRESS_SIZE],
    destination_hardware: [u8; ethernet::ADDRESS_SIZE],
    identification: u16,
    source_address: [u8; 4],
    source_port: u16,
    destination_address: [u8; 4],
    destination_port: u16,
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

    let header = ethernet::Header::new(destination_hardware, source_hardware, ethernet::TYPE_IPV4);
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
        source: source_address,
        destination: destination_address,
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
        source_port,
        destination_port,
        length: udp_length,
        checksum: 0,
    };
    header.write(datagram).ok()?;
    datagram[udp::HEADER_SIZE..].copy_from_slice(payload);
    header.checksum = udp::checksum(source_address, destination_address, datagram).ok()?;
    header.write(datagram).ok()?;

    let end = ip_start + usize::from(total_length);
    area[end..frame_length].fill(0);
    Some(frame_length)
}

/// Do nothing with a datagram: the caller is not waiting for one.
fn ignore(_: [u8; 4], _: u16, _: [u8; 4], _: u16, _: &[u8]) {}

/// Open one socket, or report that the table is full or the link is missing.
pub fn socket_open() -> Result<usize, SocketError> {
    let _guard = lock();
    let network = table();
    if network.interface.is_none() {
        return Err(SocketError::Unsupported);
    }
    let Some((index, slot)) = network
        .sockets
        .iter_mut()
        .enumerate()
        .find(|(_, socket)| !socket.open)
    else {
        return Err(SocketError::NoBuffer);
    };
    *slot = Socket::EMPTY;
    slot.open = true;
    Ok(index)
}

/// Release one socket and drop the datagrams it still holds.
pub fn socket_close(index: usize) {
    let _guard = lock();
    if let Some(slot) = table().sockets.get_mut(index) {
        *slot = Socket::EMPTY;
    }
}

/// Bind a socket to a local address and port. A port of zero picks a free one.
pub fn socket_bind(index: usize, address: [u8; 4], port: u16) -> Result<(), SocketError> {
    let _guard = lock();
    let network = table();
    let mut socket = *network.sockets.get(index).ok_or(SocketError::NotSocket)?;
    if !socket.open {
        return Err(SocketError::NotSocket);
    }
    if address != [0; 4] && address != GUEST_ADDRESS {
        return Err(SocketError::AddressNotAvailable);
    }

    let port = if port == 0 {
        allocate_port(&network.sockets).ok_or(SocketError::NoBuffer)?
    } else {
        port
    };
    if port_in_use(&network.sockets, index, port) {
        return Err(SocketError::AddressInUse);
    }

    socket.bound = true;
    socket.local_address = address;
    socket.local_port = port;
    network.sockets[index] = socket;
    Ok(())
}

/// Choose the other end of a socket.
pub fn socket_connect(index: usize, address: [u8; 4], port: u16) -> Result<(), SocketError> {
    let _guard = lock();
    let network = table();
    let mut socket = *network.sockets.get(index).ok_or(SocketError::NotSocket)?;
    if !socket.open {
        return Err(SocketError::NotSocket);
    }
    if socket.connected && (socket.remote_address != address || socket.remote_port != port) {
        return Err(SocketError::IsConnected);
    }
    if !socket.bound {
        let port = allocate_port(&network.sockets).ok_or(SocketError::NoBuffer)?;
        socket.bound = true;
        socket.local_address = GUEST_ADDRESS;
        socket.local_port = port;
    }
    socket.connected = true;
    socket.remote_address = address;
    socket.remote_port = port;
    network.sockets[index] = socket;
    Ok(())
}

/// Local address and port of a socket.
pub fn socket_local(index: usize) -> Result<([u8; 4], u16), SocketError> {
    let _guard = lock();
    let socket = table()
        .sockets
        .get(index)
        .copied()
        .ok_or(SocketError::NotSocket)?;
    if !socket.open {
        return Err(SocketError::NotSocket);
    }
    Ok((socket.local_address, socket.local_port))
}

/// Send one datagram from a socket.
///
/// An address of `None` means the socket has to be connected; a socket that
/// was never bound picks a free local port first, like Linux.
pub fn socket_send(
    index: usize,
    destination: Option<([u8; 4], u16)>,
    payload: &[u8],
) -> Result<(), SocketError> {
    let _guard = lock();
    let network = table();
    let mut socket = *network.sockets.get(index).ok_or(SocketError::NotSocket)?;
    if !socket.open {
        return Err(SocketError::NotSocket);
    }

    let (destination_address, destination_port) = match destination {
        Some(endpoint) => endpoint,
        None if socket.connected => (socket.remote_address, socket.remote_port),
        None => return Err(SocketError::NotConnected),
    };
    if !on_link(destination_address) {
        return Err(SocketError::NetworkUnreachable);
    }
    if !socket.bound {
        let port = allocate_port(&network.sockets).ok_or(SocketError::NoBuffer)?;
        socket.bound = true;
        socket.local_address = GUEST_ADDRESS;
        socket.local_port = port;
        network.sockets[index] = socket;
    }

    let source_address = if socket.local_address == [0; 4] {
        GUEST_ADDRESS
    } else {
        socket.local_address
    };
    let source_port = socket.local_port;

    let Some(interface) = network.interface.as_mut() else {
        return Err(SocketError::Unsupported);
    };
    interface
        .send(
            source_address,
            source_port,
            destination_address,
            destination_port,
            payload,
        )
        .map_err(socket_error)?;

    crate::kprintln!(
        "[net] udp send {}:{} -> {}:{} len={}",
        Address(source_address),
        source_port,
        Address(destination_address),
        destination_port,
        payload.len()
    );
    Ok(())
}

/// Receive one datagram into `buffer`, or report that none is waiting.
///
/// A datagram longer than `buffer` is truncated and the number of bytes copied
/// is returned, which is what Linux reports.
pub fn socket_receive(index: usize, buffer: &mut [u8]) -> Result<Option<Received>, SocketError> {
    let _guard = lock();
    let network = table();
    pump(network)?;

    let socket = network
        .sockets
        .get_mut(index)
        .ok_or(SocketError::NotSocket)?;
    if !socket.open {
        return Err(SocketError::NotSocket);
    }
    if socket.queued == 0 {
        return Ok(None);
    }

    let slot = socket.head;
    let Some(datagram) = socket.queue[slot].take() else {
        return Err(SocketError::Invalid);
    };
    socket.head = (socket.head + 1) % QUEUE_DEPTH;
    socket.queued -= 1;

    let count = datagram.length.min(buffer.len());
    buffer[..count].copy_from_slice(&datagram.data[..count]);
    Ok(Some(Received {
        source_address: datagram.source_address,
        source_port: datagram.source_port,
        length: count,
    }))
}

/// `true` when a datagram is waiting for the socket.
///
/// The card is polled first, so a datagram that arrived since the last call
/// makes the socket readable.
pub fn socket_readable(index: usize) -> bool {
    let _guard = lock();
    let network = table();
    let _ = pump(network);
    match network.sockets.get(index) {
        Some(socket) => socket.open && socket.queued > 0,
        None => false,
    }
}

/// `true` when the socket has an other end, so a write can make progress.
pub fn socket_writable(index: usize) -> bool {
    let _guard = lock();
    match table().sockets.get(index) {
        Some(socket) => socket.open && socket.connected,
        None => false,
    }
}

/// Read the frames that arrived and queue the datagrams by their local port.
fn pump(network: &mut Network) -> Result<usize, SocketError> {
    let Network { interface, sockets } = network;
    let Some(interface) = interface.as_mut() else {
        return Err(SocketError::Unsupported);
    };
    interface
        .poll(
            |source, source_port, destination, destination_port, payload| {
                deliver(
                    sockets,
                    source,
                    source_port,
                    destination,
                    destination_port,
                    payload,
                );
            },
        )
        .map_err(socket_error)
}

/// Hand one datagram to the socket that bound its destination port.
///
/// A datagram for a port nobody asked about is dropped with the reason: a
/// datagram that disappears without a word cannot be told apart from one that
/// never arrived.
fn deliver(
    sockets: &mut [Socket; MAX_SOCKETS],
    source_address: [u8; 4],
    source_port: u16,
    destination_address: [u8; 4],
    destination_port: u16,
    payload: &[u8],
) {
    for socket in sockets.iter_mut() {
        if !socket.open || !socket.bound || socket.local_port != destination_port {
            continue;
        }
        if socket.local_address != [0; 4] && socket.local_address != destination_address {
            continue;
        }
        if socket.connected
            && (socket.remote_address != source_address || socket.remote_port != source_port)
        {
            continue;
        }
        if socket.queued == QUEUE_DEPTH {
            crate::kprintln!(
                "[net] drop port={}: the queue of the socket is full",
                destination_port
            );
            return;
        }

        let length = payload.len().min(MAX_DATAGRAM);
        let mut data = [0u8; MAX_DATAGRAM];
        data[..length].copy_from_slice(&payload[..length]);
        let slot = (socket.head + socket.queued) % QUEUE_DEPTH;
        socket.queue[slot] = Some(Datagram {
            source_address,
            source_port,
            length,
            data,
        });
        socket.queued += 1;
        crate::kprintln!(
            "[net] udp recv {}:{} -> {}:{} len={} data=\"{}\"",
            Address(source_address),
            source_port,
            Address(destination_address),
            destination_port,
            length,
            core::str::from_utf8(&payload[..length]).unwrap_or("<binary>")
        );
        return;
    }

    crate::kprintln!("[net] drop port={}: no one asked for it", destination_port);
}

/// Port the kernel should hand to a socket that asked for zero.
fn allocate_port(sockets: &[Socket; MAX_SOCKETS]) -> Option<u16> {
    (EPHEMERAL_FIRST..=EPHEMERAL_LAST).find(|&port| !port_taken(sockets, port))
}

/// `true` when an open socket already owns `port`.
fn port_taken(sockets: &[Socket; MAX_SOCKETS], port: u16) -> bool {
    sockets
        .iter()
        .any(|socket| socket.open && socket.bound && socket.local_port == port)
}

/// `true` when another socket owns `port`.
fn port_in_use(sockets: &[Socket; MAX_SOCKETS], index: usize, port: u16) -> bool {
    sockets.iter().enumerate().any(|(other, socket)| {
        other != index && socket.open && socket.bound && socket.local_port == port
    })
}

/// `true` when `address` is inside the subnet of the guest.
fn on_link(address: [u8; 4]) -> bool {
    (0..4).all(|index| (address[index] & NETMASK[index]) == (GUEST_ADDRESS[index] & NETMASK[index]))
}

/// Turn a failure of the path into the failure of a socket call.
fn socket_error(error: Error) -> SocketError {
    match error {
        Error::GatewayTimeout => SocketError::HostUnreachable,
        Error::Refused(_) => SocketError::Invalid,
        Error::Device(_) | Error::AnswerTimeout => SocketError::HostUnreachable,
    }
}

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
