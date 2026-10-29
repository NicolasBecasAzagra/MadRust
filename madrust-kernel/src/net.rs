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
//! hardware address of a neighbour, [`ipv4`] carries the datagram or the
//! segment, and [`ethernet`] frames it. The frames themselves are built and
//! read by the crate `madrust_net`; this module only decides what to send and
//! where the answers go.
//!
//! Nothing is allocated: the transmit buffer belongs to the driver, and the
//! frames that arrive are read where the device wrote them, in the physical
//! page the driver posted. The driver is polled, because the interrupt of the
//! card is not routed.
//!
//! ## Sockets
//!
//! [`run`] installs the interface in a table of sockets after the fixed
//! demonstration exchange. A socket is a datagram socket or a stream socket,
//! and the Linux socket calls of [`crate::user`] reach the table through the
//! functions below: [`socket_open`] with the kind, [`socket_bind`],
//! [`socket_connect`], [`socket_listen`], [`socket_accept`], [`socket_send`]
//! and [`socket_send_stream`], [`socket_receive`] and
//! [`socket_receive_stream`], [`socket_shutdown`], [`socket_local`],
//! [`socket_peer`], [`socket_readable`] and [`socket_writable`].
//!
//! A datagram socket keeps its local and remote endpoints and a small queue
//! for the datagrams that arrived before the program asked for them. A stream
//! socket keeps one connection of the crate `madrust_net`, the payload of the
//! segment it sent last so the connection can ask for it again, and the moment
//! of the last timer tick it saw. The bytes of the stream are read in place
//! out of the connection, which keeps them in order. Every queue is fixed, so
//! no socket path allocates.
//!
//! ## Waiting
//!
//! The crate of TCP reads no clock: [`Stream::advance`] gives it the time
//! that passed since the last call, in milliseconds, taken from the accounting
//! tick of [`crate::pit`]. A call that has to wait for the other end (a
//! handshake, the acknowledgement of a segment, an answer or a connection to
//! accept) polls the card, applies the time to the connection, and then sleeps
//! until the next timer interrupt with the table unlocked: the wait is a
//! moment, and the retransmission is the connection's answer to it.
//!
//! The lock is dropped for every sleep and reacquired for every step, because
//! a thread that waits with the lock held would stop the timer path from
//! delivering the next frame. A datagram receive still reports a would block
//! instead of sleeping, exactly as before.

use core::cell::UnsafeCell;

use madrust_arch::cpu;
use madrust_net::tcp::{
    header, Connection, Event, Flags, Header, Response, Retransmission, Segment, State,
};
use madrust_net::{arp, ethernet, ipv4, tcp, udp};

use crate::pit;
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

/// Largest payload one segment of a stream can carry.
///
/// The link of the emulator carries 1500 bytes in a frame, and an IPv4 header
/// and a TCP header without options take twenty each.
const MAX_SEGMENT: usize = 1460;

/// Number of segments a poll of the card may leave waiting to be sent.
///
/// The segments a received packet can answer with are the ones of the
/// connections of the table, and a poll reads a handful of frames, so four
/// slots are enough; a fifth answer waits for the next tick.
const OUTGOING_DEPTH: usize = 4;

/// Accounting ticks of [`crate::pit`] that make one second.
const TICKS_PER_SECOND: u64 = pit::TICK_HZ;

/// Milliseconds of one accounting tick.
const MS_PER_TICK: u64 = 1000 / pit::TICK_HZ;

/// Time a handshake is given before `connect` reports that the peer is silent.
const CONNECT_TIMEOUT: u64 = 5 * TICKS_PER_SECOND;

/// Time `accept` waits for a connection to arrive.
const ACCEPT_TIMEOUT: u64 = 5 * TICKS_PER_SECOND;

/// Time `recv` waits for bytes of a stream before it reports a would block.
const RECEIVE_TIMEOUT: u64 = 5 * TICKS_PER_SECOND;

/// Time `send` waits for the acknowledgement of a segment.
const SEND_TIMEOUT: u64 = 5 * TICKS_PER_SECOND;

/// Time a retired stream is given to finish its closing.
const CLOSE_TIMEOUT: u64 = 2 * TICKS_PER_SECOND;

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
    /// The other end did not answer in the time the call allows.
    TimedOut,
    /// The other end refused the connection, or reset it.
    Refused,
    /// The other end reset the connection after it was established.
    Reset,
    /// The other end has gone, so there is nothing left to send.
    BrokenPipe,
}

/// Kind of socket a descriptor refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketType {
    /// One datagram at a time, with no connection of its own.
    Datagram,
    /// A connection that carries a stream of bytes.
    Stream,
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

/// One stream socket: one connection, its endpoints and the last segment sent.
struct Stream {
    /// Local address, which may be the wildcard `0.0.0.0`.
    local_address: [u8; 4],
    /// Local port, zero until a call chooses one.
    local_port: u16,
    /// Address of the other end, once it is known.
    remote_address: [u8; 4],
    /// Port of the other end, once it is known.
    remote_port: u16,
    /// The application waits for a connection on this socket.
    listening: bool,
    /// The connection of the crate, from the first call that opens one.
    connection: Option<Connection>,
    /// Sequence number of the first byte of [`Stream::sent`].
    sent_sequence: u32,
    /// Bytes of [`Stream::sent`] that are in use.
    sent_length: usize,
    /// Payload of the segment sent last, for a retransmission.
    sent: [u8; MAX_SEGMENT],
    /// Accounting tick at which the connection last saw time pass.
    last_tick: u64,
}

impl Stream {
    /// A stream that is not bound and has no connection yet.
    const fn new() -> Self {
        Self {
            local_address: [0; 4],
            local_port: 0,
            remote_address: [0; 4],
            remote_port: 0,
            listening: false,
            connection: None,
            sent_sequence: 0,
            sent_length: 0,
            sent: [0; MAX_SEGMENT],
            last_tick: 0,
        }
    }

    /// `true` when a call chose the local port.
    const fn bound(&self) -> bool {
        self.local_port != 0
    }

    /// State of the connection, or [`State::Closed`] before one exists.
    fn state(&self) -> State {
        match self.connection.as_ref() {
            Some(connection) => connection.state(),
            None => State::Closed,
        }
    }

    /// Number of payload bytes sent and not acknowledged yet.
    fn unacknowledged(&self) -> u32 {
        match self.connection.as_ref() {
            Some(connection) => connection.unacknowledged(),
            None => 0,
        }
    }

    /// Bytes the peer said it can receive.
    fn window(&self) -> u16 {
        match self.connection.as_ref() {
            Some(connection) => connection.send_window(),
            None => 0,
        }
    }

    /// Bytes of the stream that arrived and nobody read yet.
    fn ready(&self) -> &[u8] {
        match self.connection.as_ref() {
            Some(connection) => connection.received(),
            None => &[],
        }
    }

    /// Acknowledgement number the next segment of this side carries.
    fn acknowledge(&self) -> u32 {
        match self.connection.as_ref() {
            Some(connection) => connection.receive_next(),
            None => 0,
        }
    }

    /// Window the next segment of this side advertises.
    fn advertise(&self) -> u16 {
        match self.connection.as_ref() {
            Some(connection) => connection.receive_window(),
            None => 0,
        }
    }

    /// Initial sequence number of a connection that starts on this socket.
    ///
    /// The value only has to differ between connections; the tick of the
    /// clock and the local port make it differ between two runs and between
    /// two sockets of one run.
    fn initial_sequence(&self) -> u32 {
        (pit::ticks() as u32)
            .wrapping_mul(64_000)
            .wrapping_add(u32::from(self.local_port))
    }

    /// Apply the time that passed since the last call to the connection.
    fn advance(&mut self, now: u64) -> Option<Action> {
        let elapsed = now
            .saturating_sub(self.last_tick)
            .saturating_mul(MS_PER_TICK);
        if elapsed == 0 {
            return None;
        }
        self.last_tick = now;
        let elapsed = u32::try_from(elapsed).unwrap_or(u32::MAX);
        match self.connection.as_mut() {
            Some(connection) => act(connection.on(Event::Tick { elapsed }).response),
            None => None,
        }
    }

    /// Hand `bytes` to the connection, which becomes the payload to send again
    /// when the timer expires.
    fn queue(&mut self, bytes: &[u8]) -> Option<Action> {
        let length = bytes.len().min(MAX_SEGMENT);
        if length == 0 {
            return None;
        }
        let connection = self.connection.as_mut()?;
        let sequence = connection.send_next();
        self.sent[..length].copy_from_slice(&bytes[..length]);
        self.sent_sequence = sequence;
        self.sent_length = length;
        act(connection.on(Event::Send(&self.sent[..length])).response)
    }
}

/// One segment a connection asked for, without the borrow of its payload.
///
/// The connection returns a [`Response`] that borrows the bytes of the caller,
/// which live in [`Stream::sent`]. The action copies the fields of the segment
/// and leaves the payload where it is, so the stream can be read again to put
/// the segment on the wire.
#[derive(Clone, Copy)]
enum Action {
    /// Send a segment this side built; `length` bytes of [`Stream::sent`] are
    /// its payload.
    Send {
        /// Sequence number of the first byte.
        sequence: u32,
        /// Flags of the segment.
        flags: Flags,
        /// Bytes of payload after the header.
        length: usize,
    },
    /// Send the payload of the last segment again.
    Retransmit(Retransmission),
    /// The connection gave up.
    Aborted,
}

/// Turn what the connection answered into an [`Action`].
fn act(response: Option<Response<'_>>) -> Option<Action> {
    match response? {
        Response::Send(segment) => Some(Action::Send {
            sequence: segment.header.sequence,
            flags: segment.header.flags,
            length: segment.payload.len(),
        }),
        Response::Retransmit(again) => Some(Action::Retransmit(again)),
        Response::Aborted => Some(Action::Aborted),
    }
}

/// A segment one poll wants to put on the wire once the frame is written.
#[derive(Clone, Copy)]
struct Outgoing {
    /// Address of this side of the connection.
    source_address: [u8; 4],
    /// Address of the other side.
    destination_address: [u8; 4],
    /// The header to send; the segments a received packet answers with carry
    /// no payload.
    segment: Segment<'static>,
}

/// One packet a frame carried and this kernel understands.
enum Packet<'a> {
    /// One datagram of UDP.
    Datagram {
        /// Address the datagram came from.
        source_address: [u8; 4],
        /// Port the datagram came from.
        source_port: u16,
        /// Address the datagram was sent to.
        destination_address: [u8; 4],
        /// Port the datagram was sent to.
        destination_port: u16,
        /// The bytes after the header of UDP.
        payload: &'a [u8],
    },
    /// One segment of TCP.
    Segment {
        /// Address the segment came from.
        source_address: [u8; 4],
        /// Address the segment was sent to.
        destination_address: [u8; 4],
        /// The header and the payload of the segment.
        segment: Segment<'a>,
    },
}

/// The interface and the sockets that share it.
struct Network {
    interface: Option<Interface>,
    sockets: [Socket; MAX_SOCKETS],
    streams: [Option<Stream>; MAX_SOCKETS],
}

impl Network {
    /// A table with no interface and no socket.
    const fn new() -> Self {
        Self {
            interface: None,
            sockets: [Socket::EMPTY; MAX_SOCKETS],
            streams: [None, None, None, None, None, None, None, None],
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
/// answer. Then keep the interface for the socket calls of both kinds.
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
            self.poll(|packet| {
                let Packet::Datagram {
                    source_address: source,
                    source_port,
                    destination_address: destination,
                    destination_port,
                    payload,
                } = packet
                else {
                    return;
                };
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
            })?;
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
        let hardware = self.hardware(destination_address)?;
        let identification = self.identification;
        self.identification = self.identification.wrapping_add(1);
        let local_hardware = self.device.mac();

        self.device.transmit(|area| {
            build_udp(
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

    /// Put one segment of TCP on the wire.
    ///
    /// The checksum needs both addresses, so the segment arrives with it at
    /// zero and the caller wrote it from the addresses of the connection.
    fn send_tcp(
        &mut self,
        source_address: [u8; 4],
        destination_address: [u8; 4],
        segment: &Segment<'_>,
    ) -> Result<(), Error> {
        let hardware = self.hardware(destination_address)?;
        let identification = self.identification;
        self.identification = self.identification.wrapping_add(1);
        let local_hardware = self.device.mac();

        self.device.transmit(|area| {
            build_tcp(
                area,
                local_hardware,
                hardware,
                identification,
                source_address,
                destination_address,
                segment,
            )
        })?;
        Ok(())
    }

    /// Hardware address of `address`, learned now if it is not known yet.
    ///
    /// A neighbour that is already known is used without reading the card
    /// first: a segment that arrived between two sends would otherwise be
    /// dropped by the read of the resolution.
    fn hardware(&mut self, address: [u8; 4]) -> Result<[u8; ethernet::ADDRESS_SIZE], Error> {
        match self.known(address) {
            Some(hardware) => Ok(hardware),
            None => self.resolve(address),
        }
    }

    /// Read the frames that arrived and hand them to `deliver`.
    ///
    /// Returns the number of frames that were read.
    fn poll(&mut self, mut deliver: impl FnMut(Packet<'_>)) -> Result<usize, Error> {
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
    deliver: &mut impl FnMut(Packet<'_>),
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

/// Take an IPv4 datagram apart and deliver it when it carries UDP or TCP.
fn receive(payload: &[u8], deliver: &mut impl FnMut(Packet<'_>)) {
    let packet = match ipv4::parse(payload) {
        Ok(packet) => packet,
        Err(reason) => {
            crate::kprintln!("[net] drop: {}", reason);
            return;
        }
    };

    match packet.header.protocol {
        ipv4::PROTOCOL_UDP => {
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
            deliver(Packet::Datagram {
                source_address: packet.header.source,
                source_port: datagram.header.source_port,
                destination_address: packet.header.destination,
                destination_port: datagram.header.destination_port,
                payload: datagram.payload,
            });
        }
        ipv4::PROTOCOL_TCP => {
            let segment = match tcp::Segment::parse(packet.payload) {
                Ok(segment) => segment,
                Err(reason) => {
                    crate::kprintln!("[net] drop: {}", reason);
                    return;
                }
            };
            // The checksum covers the header exactly as it arrived, options
            // included: the crate re-encodes a header without them, so the
            // check runs over the bytes of the frame.
            if let Err(reason) = header::verify(
                packet.header.source,
                packet.header.destination,
                packet.payload,
            ) {
                crate::kprintln!("[net] drop: {}", reason);
                return;
            }
            deliver(Packet::Segment {
                source_address: packet.header.source,
                destination_address: packet.header.destination,
                segment,
            });
        }
        other => crate::kprintln!(
            "[net] drop protocol={}: not a UDP datagram or a TCP segment",
            other
        ),
    }
}

/// Write the Ethernet header and the IPv4 header of one frame.
///
/// Returns where the IPv4 header starts, where the header of the next protocol
/// starts and the length of the whole frame with its padding. Every header is
/// computed and written by the crate; the frame goes to the neighbour, so its
/// hardware address is the destination of the frame while the address of the
/// datagram is the destination of the call.
#[allow(clippy::too_many_arguments)]
fn open_frame(
    area: &mut [u8],
    source_hardware: [u8; ethernet::ADDRESS_SIZE],
    destination_hardware: [u8; ethernet::ADDRESS_SIZE],
    identification: u16,
    source_address: [u8; 4],
    destination_address: [u8; 4],
    protocol: u8,
    payload_length: usize,
) -> Option<(usize, usize, usize)> {
    let total_length = ipv4::MIN_HEADER_SIZE.checked_add(payload_length)?;
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
        protocol,
        checksum: 0,
        source: source_address,
        destination: destination_address,
    };
    header.fill_checksum().ok()?;
    let ip_start = ethernet::HEADER_SIZE;
    header.write(&mut area[ip_start..]).ok()?;

    Some((ip_start, ip_start + ipv4::MIN_HEADER_SIZE, frame_length))
}

/// Write one datagram into `area` and return the length of the whole frame.
///
/// Returns `None` when a length does not fit its field or the frame does not
/// fit the buffer of the device.
#[allow(clippy::too_many_arguments)]
fn build_udp(
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
    let (ip_start, udp_start, frame_length) = open_frame(
        area,
        source_hardware,
        destination_hardware,
        identification,
        source_address,
        destination_address,
        ipv4::PROTOCOL_UDP,
        usize::from(udp_length),
    )?;

    // The checksum of UDP covers the header with its checksum field at zero and
    // the payload, so the datagram is written first and the field is filled
    // once the bytes are in place.
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

    let end = ip_start + ipv4::MIN_HEADER_SIZE + usize::from(udp_length);
    area[end..frame_length].fill(0);
    Some(frame_length)
}

/// Write one segment of TCP into `area` and return the length of the whole
/// frame.
fn build_tcp(
    area: &mut [u8],
    source_hardware: [u8; ethernet::ADDRESS_SIZE],
    destination_hardware: [u8; ethernet::ADDRESS_SIZE],
    identification: u16,
    source_address: [u8; 4],
    destination_address: [u8; 4],
    segment: &Segment<'_>,
) -> Option<usize> {
    let length = segment
        .header
        .header_size()
        .checked_add(segment.payload.len())?;
    let (ip_start, segment_start, frame_length) = open_frame(
        area,
        source_hardware,
        destination_hardware,
        identification,
        source_address,
        destination_address,
        ipv4::PROTOCOL_TCP,
        length,
    )?;

    segment.write(&mut area[segment_start..]).ok()?;

    let end = ip_start + ipv4::MIN_HEADER_SIZE + length;
    area[end..frame_length].fill(0);
    Some(frame_length)
}

/// Do nothing with a packet: the caller is not waiting for one.
fn ignore(_: Packet<'_>) {}

/// Open one socket of `kind`, or report that the table is full or the link is
/// missing.
pub fn socket_open(kind: SocketType) -> Result<usize, SocketError> {
    let _guard = lock();
    let network = table();
    if network.interface.is_none() {
        return Err(SocketError::Unsupported);
    }
    let Some(index) = (0..MAX_SOCKETS)
        .find(|&index| !network.sockets[index].open && network.streams[index].is_none())
    else {
        return Err(SocketError::NoBuffer);
    };
    let Some(slot) = network.sockets.get_mut(index) else {
        return Err(SocketError::NoBuffer);
    };
    *slot = Socket::EMPTY;
    let Some(stream) = network.streams.get_mut(index) else {
        return Err(SocketError::NoBuffer);
    };
    match kind {
        SocketType::Datagram => slot.open = true,
        SocketType::Stream => *stream = Some(Stream::new()),
    }
    Ok(index)
}

/// Release one socket and drop what it still holds.
///
/// A stream that is not closed yet says goodbye: the FIN goes out and the
/// answer is given the time of [`CLOSE_TIMEOUT`], so the other end learns that
/// this side is gone. The slot is freed whether the answer arrives or not.
pub fn socket_close(index: usize) {
    let deadline = pit::ticks() + CLOSE_TIMEOUT;

    // A datagram socket holds nothing that outlives it, and a stream says
    // goodbye once, before the slot is freed.
    {
        let _guard = lock();
        let network = table();
        let Network {
            interface,
            sockets,
            streams,
        } = network;
        let Some(stream) = streams.get_mut(index).and_then(Option::as_mut) else {
            if let Some(slot) = sockets.get_mut(index) {
                *slot = Socket::EMPTY;
            }
            return;
        };
        if let Some(interface) = interface.as_mut() {
            let response = close_response(stream);
            if let Some(response) = response {
                let _ = emit(interface, stream, response);
            }
        }
    }

    loop {
        let finished = {
            let _guard = lock();
            let network = table();
            if network.streams.get(index).is_none_or(Option::is_none) {
                true
            } else {
                let _ = pump(network);
                let Network {
                    interface, streams, ..
                } = network;
                match streams.get_mut(index).and_then(Option::as_mut) {
                    None => true,
                    Some(stream) => {
                        let response = stream.advance(pit::ticks());
                        if let (Some(interface), Some(response)) = (interface.as_mut(), response) {
                            let _ = emit(interface, stream, response);
                        }
                        matches!(stream.state(), State::Closed | State::TimeWait)
                    }
                }
            }
        };
        if finished || pit::ticks() >= deadline {
            break;
        }
        pause();
    }

    let _guard = lock();
    let Network {
        sockets, streams, ..
    } = table();
    if let Some(stream) = streams.get_mut(index) {
        *stream = None;
    }
    if let Some(slot) = sockets.get_mut(index) {
        *slot = Socket::EMPTY;
    }
}

/// The segment that closes the stream of a socket, if it has a connection.
fn close_response(stream: &mut Stream) -> Option<Action> {
    match stream.connection.as_mut() {
        Some(connection) => act(connection.on(Event::Close).response),
        None => None,
    }
}

/// Kind of socket a slot holds.
pub fn socket_type(index: usize) -> Result<SocketType, SocketError> {
    let _guard = lock();
    let network = table();
    kind_in(&network.sockets, &network.streams, index)
}

/// Kind of socket a slot holds, from a table the caller already locked.
fn kind_in(
    sockets: &[Socket; MAX_SOCKETS],
    streams: &[Option<Stream>; MAX_SOCKETS],
    index: usize,
) -> Result<SocketType, SocketError> {
    if streams.get(index).is_some_and(Option::is_some) {
        return Ok(SocketType::Stream);
    }
    match sockets.get(index) {
        Some(socket) if socket.open => Ok(SocketType::Datagram),
        _ => Err(SocketError::NotSocket),
    }
}

/// Bind a socket to a local address and port. A port of zero picks a free one.
pub fn socket_bind(index: usize, address: [u8; 4], port: u16) -> Result<(), SocketError> {
    let _guard = lock();
    let network = table();
    let kind = kind_in(&network.sockets, &network.streams, index)?;
    if address != [0; 4] && address != GUEST_ADDRESS {
        return Err(SocketError::AddressNotAvailable);
    }

    let port = if port == 0 {
        allocate_port(&network.sockets, &network.streams).ok_or(SocketError::NoBuffer)?
    } else {
        port
    };
    if port_in_use(&network.sockets, &network.streams, index, port) {
        return Err(SocketError::AddressInUse);
    }

    match kind {
        SocketType::Datagram => {
            let socket = network
                .sockets
                .get_mut(index)
                .ok_or(SocketError::NotSocket)?;
            socket.bound = true;
            socket.local_address = address;
            socket.local_port = port;
        }
        SocketType::Stream => {
            let stream = network
                .streams
                .get_mut(index)
                .and_then(Option::as_mut)
                .ok_or(SocketError::NotSocket)?;
            if stream.connection.is_some() {
                return Err(SocketError::Invalid);
            }
            stream.local_address = address;
            stream.local_port = port;
        }
    }
    Ok(())
}

/// Choose the other end of a socket.
pub fn socket_connect(index: usize, address: [u8; 4], port: u16) -> Result<(), SocketError> {
    let kind = {
        let _guard = lock();
        let network = table();
        kind_in(&network.sockets, &network.streams, index)?
    };
    match kind {
        SocketType::Datagram => connect_datagram(index, address, port),
        SocketType::Stream => connect_stream(index, address, port),
    }
}

/// Choose the other end of a datagram socket.
fn connect_datagram(index: usize, address: [u8; 4], port: u16) -> Result<(), SocketError> {
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
        let port =
            allocate_port(&network.sockets, &network.streams).ok_or(SocketError::NoBuffer)?;
        socket.bound = true;
        socket.local_address = GUEST_ADDRESS;
        socket.local_port = port;
    }
    socket.connected = true;
    socket.remote_address = address;
    socket.remote_port = port;
    if let Some(slot) = network.sockets.get_mut(index) {
        *slot = socket;
    }
    Ok(())
}

/// Start the handshake of a stream and wait until the connection is
/// established.
fn connect_stream(index: usize, address: [u8; 4], port: u16) -> Result<(), SocketError> {
    {
        let _guard = lock();
        let network = table();
        let Network {
            interface,
            sockets,
            streams,
        } = network;
        if !on_link(address) {
            return Err(SocketError::NetworkUnreachable);
        }

        // The port is chosen before the stream is borrowed, because the choice
        // reads every socket of the table.
        let chosen = match streams.get(index).and_then(Option::as_ref) {
            Some(stream) if !stream.bound() => {
                Some(allocate_port(sockets, streams).ok_or(SocketError::NoBuffer)?)
            }
            Some(_) => None,
            None => return Err(SocketError::NotSocket),
        };

        let Some(stream) = streams.get_mut(index).and_then(Option::as_mut) else {
            return Err(SocketError::NotSocket);
        };
        if let Some(connection) = stream.connection.as_ref() {
            if stream.remote_address == address
                && stream.remote_port == port
                && connection.state() == State::Established
            {
                return Ok(());
            }
            return Err(SocketError::IsConnected);
        }
        if let Some(chosen) = chosen {
            stream.local_address = GUEST_ADDRESS;
            stream.local_port = chosen;
        }
        let Some(interface) = interface.as_mut() else {
            return Err(SocketError::Unsupported);
        };

        stream.remote_address = address;
        stream.remote_port = port;
        stream.last_tick = pit::ticks();
        let sequence = stream.initial_sequence();
        let mut connection = Connection::new(stream.local_port, port);
        let action = act(connection.on(Event::Open { sequence }).response);
        stream.connection = Some(connection);
        if let Some(action) = action {
            emit(interface, stream, action)?;
        }
    }

    wait_connected(index)
}

/// Wait until the connection of a stream is established.
fn wait_connected(index: usize) -> Result<(), SocketError> {
    let deadline = pit::ticks() + CONNECT_TIMEOUT;
    loop {
        {
            let _guard = lock();
            let network = table();
            let _ = pump(network);
            let Network {
                interface, streams, ..
            } = network;
            let Some(stream) = streams.get_mut(index).and_then(Option::as_mut) else {
                return Err(SocketError::NotSocket);
            };
            let response = stream.advance(pit::ticks());
            if let (Some(interface), Some(response)) = (interface.as_mut(), response) {
                emit(interface, stream, response)?;
            }
            match stream.state() {
                State::Established => return Ok(()),
                State::Closed => return Err(SocketError::Refused),
                _ => {}
            }
        }
        if pit::ticks() >= deadline {
            return Err(SocketError::TimedOut);
        }
        pause();
    }
}

/// Mark a stream socket as listening for a connection.
pub fn socket_listen(index: usize) -> Result<(), SocketError> {
    let _guard = lock();
    let network = table();
    if network.streams.get(index).is_none_or(Option::is_none) {
        return if network.sockets.get(index).is_some_and(|socket| socket.open) {
            Err(SocketError::Unsupported)
        } else {
            Err(SocketError::NotSocket)
        };
    }

    let chosen = match network.streams.get(index).and_then(Option::as_ref) {
        Some(stream) if !stream.bound() => {
            Some(allocate_port(&network.sockets, &network.streams).ok_or(SocketError::NoBuffer)?)
        }
        _ => None,
    };

    let Some(stream) = network.streams.get_mut(index).and_then(Option::as_mut) else {
        return Err(SocketError::NotSocket);
    };
    if stream.connection.is_some() {
        return Err(SocketError::Invalid);
    }
    if let Some(chosen) = chosen {
        stream.local_address = GUEST_ADDRESS;
        stream.local_port = chosen;
    }
    let sequence = stream.initial_sequence();
    let mut connection = Connection::new(stream.local_port, 0);
    let _ = connection.on(Event::Listen { sequence });
    stream.connection = Some(connection);
    stream.listening = true;
    stream.last_tick = pit::ticks();
    Ok(())
}

/// Wait for a connection of a listening stream and return the slot it moves to.
///
/// The listening socket keeps its local port and listens again, so the same
/// descriptor answers the next connection. The accepted socket carries the
/// connection the handshake left in [`State::Established`].
pub fn socket_accept(index: usize) -> Result<usize, SocketError> {
    let deadline = pit::ticks() + ACCEPT_TIMEOUT;
    loop {
        let accepted = {
            let _guard = lock();
            let network = table();
            let _ = pump(network);
            let Network {
                interface, streams, ..
            } = network;
            let Some(stream) = streams.get_mut(index).and_then(Option::as_mut) else {
                return Err(SocketError::NotSocket);
            };
            if !stream.listening {
                return Err(SocketError::Invalid);
            }
            let response = stream.advance(pit::ticks());
            if let (Some(interface), Some(response)) = (interface.as_mut(), response) {
                let _ = emit(interface, stream, response);
            }
            if stream.state() == State::Established {
                let connection = stream.connection.take();
                let endpoints = (
                    stream.local_address,
                    stream.local_port,
                    stream.remote_address,
                    stream.remote_port,
                    stream.last_tick,
                );
                let sequence = stream.initial_sequence();
                let mut listening = Connection::new(stream.local_port, 0);
                let _ = listening.on(Event::Listen { sequence });
                stream.connection = Some(listening);
                connection.map(|connection| (connection, endpoints))
            } else {
                None
            }
        };

        if let Some((connection, endpoints)) = accepted {
            let _guard = lock();
            let network = table();
            let Some(new_index) = (0..MAX_SOCKETS)
                .find(|&index| !network.sockets[index].open && network.streams[index].is_none())
            else {
                return Err(SocketError::NoBuffer);
            };
            let (local_address, local_port, remote_address, remote_port, last_tick) = endpoints;
            let mut stream = Stream::new();
            stream.local_address = local_address;
            stream.local_port = local_port;
            stream.remote_address = remote_address;
            stream.remote_port = remote_port;
            stream.last_tick = last_tick;
            stream.connection = Some(connection);
            if let Some(slot) = network.sockets.get_mut(new_index) {
                *slot = Socket::EMPTY;
            }
            if let Some(slot) = network.streams.get_mut(new_index) {
                *slot = Some(stream);
            }
            return Ok(new_index);
        }
        if pit::ticks() >= deadline {
            return Err(SocketError::TimedOut);
        }
        pause();
    }
}

/// Local address and port of a socket.
pub fn socket_local(index: usize) -> Result<([u8; 4], u16), SocketError> {
    let _guard = lock();
    let network = table();
    match kind_in(&network.sockets, &network.streams, index)? {
        SocketType::Datagram => {
            let socket = network.sockets.get(index).ok_or(SocketError::NotSocket)?;
            Ok((socket.local_address, socket.local_port))
        }
        SocketType::Stream => {
            let stream = network
                .streams
                .get(index)
                .and_then(Option::as_ref)
                .ok_or(SocketError::NotSocket)?;
            Ok((stream.local_address, stream.local_port))
        }
    }
}

/// Address and port of the other end of a socket.
///
/// A datagram socket that was never connected reports the wildcard address and
/// port zero, which is what `recvfrom` writes for it.
pub fn socket_peer(index: usize) -> Result<([u8; 4], u16), SocketError> {
    let _guard = lock();
    let network = table();
    match kind_in(&network.sockets, &network.streams, index)? {
        SocketType::Datagram => {
            let socket = network.sockets.get(index).ok_or(SocketError::NotSocket)?;
            if socket.connected {
                Ok((socket.remote_address, socket.remote_port))
            } else {
                Ok(([0; 4], 0))
            }
        }
        SocketType::Stream => {
            let stream = network
                .streams
                .get(index)
                .and_then(Option::as_ref)
                .ok_or(SocketError::NotSocket)?;
            Ok((stream.remote_address, stream.remote_port))
        }
    }
}

/// Send one datagram from a datagram socket.
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
        let port =
            allocate_port(&network.sockets, &network.streams).ok_or(SocketError::NoBuffer)?;
        socket.bound = true;
        socket.local_address = GUEST_ADDRESS;
        socket.local_port = port;
        if let Some(slot) = network.sockets.get_mut(index) {
            *slot = socket;
        }
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

/// Send `bytes` on the stream of a socket and return how many were sent.
///
/// One segment is in flight at a time, so the call waits for the
/// acknowledgement of each one and gives the connection the time it needs to
/// send a segment again. A segment carries at most [`MAX_SEGMENT`] bytes and no
/// more than the window the peer advertised.
pub fn socket_send_stream(index: usize, bytes: &[u8]) -> Result<usize, SocketError> {
    if bytes.is_empty() {
        let _guard = lock();
        let network = table();
        let stream = match network.streams.get(index).and_then(Option::as_ref) {
            Some(stream) => stream,
            None => return Err(SocketError::NotSocket),
        };
        if stream.connection.is_none() {
            return Err(SocketError::NotConnected);
        }
        return match stream.state() {
            State::Established | State::CloseWait => Ok(0),
            State::Closed => Err(SocketError::BrokenPipe),
            _ => Err(SocketError::NotConnected),
        };
    }

    let deadline = pit::ticks() + SEND_TIMEOUT;
    let mut sent = 0;
    while sent < bytes.len() {
        {
            let _guard = lock();
            let network = table();
            let _ = pump(network);
            let Network {
                interface, streams, ..
            } = network;
            let Some(stream) = streams.get_mut(index).and_then(Option::as_mut) else {
                return Err(SocketError::NotSocket);
            };
            if stream.connection.is_none() {
                return Err(SocketError::NotConnected);
            }
            let response = stream.advance(pit::ticks());
            if let (Some(interface), Some(response)) = (interface.as_mut(), response) {
                emit(interface, stream, response)?;
            }
            match stream.state() {
                State::Established | State::CloseWait => {}
                State::Closed
                | State::FinWait1
                | State::FinWait2
                | State::Closing
                | State::LastAck
                | State::TimeWait => return Err(SocketError::BrokenPipe),
                State::Listen | State::SynSent | State::SynReceived => {
                    return Err(SocketError::NotConnected)
                }
            }

            if stream.unacknowledged() == 0 {
                let room = usize::from(stream.window());
                if room > 0 {
                    let length = (bytes.len() - sent).min(MAX_SEGMENT).min(room);
                    let response = stream.queue(&bytes[sent..sent + length]);
                    if let Some(response) = response {
                        let Some(interface) = interface.as_mut() else {
                            return Err(SocketError::Unsupported);
                        };
                        emit(interface, stream, response)?;
                        sent += length;
                    }
                }
            }
        }
        if sent < bytes.len() {
            if pit::ticks() >= deadline {
                return Err(SocketError::TimedOut);
            }
            pause();
        }
    }
    Ok(sent)
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

/// Receive bytes of a stream into `buffer`.
///
/// `Some(count)` is the number of bytes copied, and zero means that the other
/// end closed and nothing is left; `None` is a would block after
/// [`RECEIVE_TIMEOUT`]. A segment longer than `buffer` is cut: the bytes that
/// did not fit stay in the connection for the next call, and the count is what
/// the program received.
pub fn socket_receive_stream(
    index: usize,
    buffer: &mut [u8],
) -> Result<Option<usize>, SocketError> {
    if buffer.is_empty() {
        return Ok(Some(0));
    }
    let deadline = pit::ticks() + RECEIVE_TIMEOUT;
    loop {
        {
            let _guard = lock();
            let network = table();
            let _ = pump(network);
            let Network {
                interface, streams, ..
            } = network;
            let Some(stream) = streams.get_mut(index).and_then(Option::as_mut) else {
                return Err(SocketError::NotSocket);
            };
            if stream.connection.is_none() {
                return Err(SocketError::NotConnected);
            }
            let response = stream.advance(pit::ticks());
            if let (Some(interface), Some(response)) = (interface.as_mut(), response) {
                let _ = emit(interface, stream, response);
            }
            let count = {
                let ready = stream.ready();
                let count = ready.len().min(buffer.len());
                if count > 0 {
                    buffer[..count].copy_from_slice(&ready[..count]);
                }
                count
            };
            if count > 0 {
                if let Some(connection) = stream.connection.as_mut() {
                    connection.consume(count);
                }
                return Ok(Some(count));
            }
            match stream.state() {
                State::Closed => return Err(SocketError::Reset),
                State::CloseWait | State::Closing | State::LastAck | State::TimeWait => {
                    return Ok(Some(0));
                }
                _ => {}
            }
        }
        if pit::ticks() >= deadline {
            return Ok(None);
        }
        pause();
    }
}

/// Close one direction of a stream, or accept the request of a datagram.
///
/// `how` is the Linux argument: zero is the read direction, which changes
/// nothing here, and one or two send the FIN of the stream.
pub fn socket_shutdown(index: usize, how: i64) -> Result<(), SocketError> {
    if how == 0 {
        return Ok(());
    }
    let _guard = lock();
    let network = table();
    let Network {
        interface,
        sockets,
        streams,
    } = network;
    let Some(stream) = streams.get_mut(index).and_then(Option::as_mut) else {
        return match sockets.get(index) {
            Some(socket) if socket.open => Ok(()),
            _ => Err(SocketError::NotSocket),
        };
    };
    let Some(interface) = interface.as_mut() else {
        return Err(SocketError::Unsupported);
    };
    let response = close_response(stream);
    if let Some(response) = response {
        emit(interface, stream, response)?;
    }
    Ok(())
}

/// `true` when a read of the socket can return data or the end of the stream.
///
/// The card is polled first, so a packet that arrived since the last call
/// makes the socket readable.
pub fn socket_readable(index: usize) -> bool {
    let _guard = lock();
    let network = table();
    let _ = pump(network);
    if let Some(stream) = network.streams.get(index).and_then(Option::as_ref) {
        if !stream.ready().is_empty() {
            return true;
        }
        return matches!(
            stream.state(),
            State::Closed | State::CloseWait | State::Closing | State::LastAck | State::TimeWait
        );
    }
    match network.sockets.get(index) {
        Some(socket) => socket.open && socket.queued > 0,
        None => false,
    }
}

/// `true` when a write of the socket can make progress.
pub fn socket_writable(index: usize) -> bool {
    let _guard = lock();
    let network = table();
    if let Some(stream) = network.streams.get(index).and_then(Option::as_ref) {
        return matches!(stream.state(), State::Established | State::CloseWait);
    }
    match network.sockets.get(index) {
        Some(socket) => socket.open && socket.connected,
        None => false,
    }
}

/// Sleep until the next timer interrupt, with the table unlocked.
///
/// The system call arrives with interrupts masked and the lock is already
/// dropped here, so enabling them lets the timer run: the call resumes on the
/// next tick, or earlier when another thread wakes the program.
fn pause() {
    cpu::enable_interrupts();
    cpu::hlt();
    cpu::disable_interrupts();
}

/// Read the frames that arrived, queue the datagrams, feed the segments to
/// their connections, and send the segments the connections answered with.
fn pump(network: &mut Network) -> Result<usize, SocketError> {
    let Network {
        interface,
        sockets,
        streams,
    } = network;
    let Some(interface) = interface.as_mut() else {
        return Err(SocketError::Unsupported);
    };

    // A segment that arrives is answered from inside the poll of the card, and
    // the device is busy until the poll returns. The answer is kept here and
    // put on the wire once the card is free.
    let mut outgoing: [Option<Outgoing>; OUTGOING_DEPTH] = [None; OUTGOING_DEPTH];
    let mut queued = 0;
    let frames = interface
        .poll(|packet| match packet {
            Packet::Datagram {
                source_address,
                source_port,
                destination_address,
                destination_port,
                payload,
            } => deliver(
                sockets,
                source_address,
                source_port,
                destination_address,
                destination_port,
                payload,
            ),
            Packet::Segment {
                source_address,
                destination_address,
                segment,
            } => deliver_segment(
                streams,
                &mut outgoing,
                &mut queued,
                source_address,
                destination_address,
                segment,
            ),
        })
        .map_err(socket_error)?;

    for item in outgoing.iter().flatten() {
        let mut segment = item.segment;
        let _ = transmit(
            interface,
            item.source_address,
            item.destination_address,
            &mut segment,
        );
    }
    Ok(frames)
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

/// Hand one segment to the connection that owns its endpoints.
fn deliver_segment(
    streams: &mut [Option<Stream>; MAX_SOCKETS],
    outgoing: &mut [Option<Outgoing>; OUTGOING_DEPTH],
    queued: &mut usize,
    source_address: [u8; 4],
    destination_address: [u8; 4],
    segment: Segment<'_>,
) {
    let header = segment.header;
    for stream in streams.iter_mut().flatten() {
        if stream.local_port != header.destination_port {
            continue;
        }
        if stream.local_address != [0; 4] && stream.local_address != destination_address {
            continue;
        }

        // A listener answers the SYN of anyone; every other state only takes a
        // segment of the connection it already has.
        let opening = stream.listening
            && stream.state() == State::Listen
            && header.flags.contains(Flags::SYN)
            && !header.flags.contains(Flags::ACK);
        let peer =
            stream.remote_address == source_address && stream.remote_port == header.source_port;
        if !opening && !peer {
            continue;
        }
        if opening {
            stream.remote_address = source_address;
            stream.remote_port = header.source_port;
            stream.last_tick = pit::ticks();
            let sequence = stream.initial_sequence();
            let mut connection = Connection::new(stream.local_port, header.source_port);
            let _ = connection.on(Event::Listen { sequence });
            stream.connection = Some(connection);
        }

        let response = match stream.connection.as_mut() {
            Some(connection) => connection.on(Event::Receive(segment)).response,
            None => None,
        };
        log_segment("recv", source_address, destination_address, &segment);
        if let Some(response) = response {
            enqueue(outgoing, queued, stream, source_address, response);
        }
        return;
    }

    crate::kprintln!(
        "[net] drop port={}: no one asked for it",
        header.destination_port
    );
}

/// Keep the segment an arriving packet asked for until the card is free.
fn enqueue(
    outgoing: &mut [Option<Outgoing>; OUTGOING_DEPTH],
    queued: &mut usize,
    stream: &Stream,
    destination_address: [u8; 4],
    response: Response<'_>,
) {
    let Response::Send(segment) = response else {
        return;
    };
    if *queued == OUTGOING_DEPTH {
        return;
    }
    outgoing[*queued] = Some(Outgoing {
        source_address: if stream.local_address == [0; 4] {
            GUEST_ADDRESS
        } else {
            stream.local_address
        },
        destination_address,
        // A segment that answers a packet carries no payload, so the bytes of
        // the received frame do not have to outlive the poll.
        segment: Segment {
            header: segment.header,
            payload: &[],
        },
    });
    *queued += 1;
}

/// Put the segment a connection asked for on the wire.
fn emit(interface: &mut Interface, stream: &Stream, action: Action) -> Result<(), SocketError> {
    let source_address = if stream.local_address == [0; 4] {
        GUEST_ADDRESS
    } else {
        stream.local_address
    };
    let destination_address = stream.remote_address;
    match action {
        Action::Send {
            sequence,
            flags,
            length,
        } => {
            let length = length.min(stream.sent_length);
            let mut segment = Segment {
                header: Header::new(
                    stream.local_port,
                    stream.remote_port,
                    sequence,
                    stream.acknowledge(),
                    flags,
                    stream.advertise(),
                ),
                payload: &stream.sent[..length],
            };
            transmit(interface, source_address, destination_address, &mut segment)
        }
        Action::Retransmit(again) => {
            let offset = usize::try_from(again.sequence.wrapping_sub(stream.sent_sequence))
                .unwrap_or(usize::MAX);
            let length = if offset > stream.sent_length {
                0
            } else {
                again.payload_length.min(stream.sent_length - offset)
            };
            let payload = if length == 0 {
                &[][..]
            } else {
                &stream.sent[offset..offset + length]
            };
            let mut segment = Segment {
                header: Header::new(
                    stream.local_port,
                    stream.remote_port,
                    again.sequence,
                    stream.acknowledge(),
                    again.flags,
                    stream.advertise(),
                ),
                payload,
            };
            transmit(interface, source_address, destination_address, &mut segment)
        }
        Action::Aborted => Err(SocketError::TimedOut),
    }
}

/// Compute the checksum of a segment, put it on the wire and report it.
fn transmit(
    interface: &mut Interface,
    source_address: [u8; 4],
    destination_address: [u8; 4],
    segment: &mut Segment<'_>,
) -> Result<(), SocketError> {
    segment
        .fill_checksum(source_address, destination_address)
        .map_err(|_| SocketError::Invalid)?;
    interface
        .send_tcp(source_address, destination_address, segment)
        .map_err(socket_error)?;
    log_segment("send", source_address, destination_address, segment);
    Ok(())
}

/// Report one segment of a stream, with the bytes it carries when it has any.
fn log_segment(
    direction: &str,
    source_address: [u8; 4],
    destination_address: [u8; 4],
    segment: &Segment<'_>,
) {
    let header = segment.header;
    let source_port = header.source_port;
    let destination_port = header.destination_port;
    let flags = header.flags.bits();
    let sequence = header.sequence;
    let acknowledgement = header.acknowledgement;
    if segment.payload.is_empty() {
        crate::kprintln!(
            "[net] tcp {} {}:{} -> {}:{} flags=0x{:02x} seq={} ack={} len=0",
            direction,
            Address(source_address),
            source_port,
            Address(destination_address),
            destination_port,
            flags,
            sequence,
            acknowledgement
        );
    } else {
        crate::kprintln!(
            "[net] tcp {} {}:{} -> {}:{} flags=0x{:02x} seq={} ack={} len={} data=\"{}\"",
            direction,
            Address(source_address),
            source_port,
            Address(destination_address),
            destination_port,
            flags,
            sequence,
            acknowledgement,
            segment.payload.len(),
            core::str::from_utf8(segment.payload).unwrap_or("<binary>")
        );
    }
}

/// Port the kernel should hand to a socket that asked for zero.
fn allocate_port(
    sockets: &[Socket; MAX_SOCKETS],
    streams: &[Option<Stream>; MAX_SOCKETS],
) -> Option<u16> {
    (EPHEMERAL_FIRST..=EPHEMERAL_LAST).find(|&port| !port_taken(sockets, streams, port))
}

/// `true` when an open socket already owns `port`.
fn port_taken(
    sockets: &[Socket; MAX_SOCKETS],
    streams: &[Option<Stream>; MAX_SOCKETS],
    port: u16,
) -> bool {
    sockets
        .iter()
        .any(|socket| socket.open && socket.bound && socket.local_port == port)
        || streams
            .iter()
            .flatten()
            .any(|stream| stream.bound() && stream.local_port == port)
}

/// `true` when another socket owns `port`.
fn port_in_use(
    sockets: &[Socket; MAX_SOCKETS],
    streams: &[Option<Stream>; MAX_SOCKETS],
    index: usize,
    port: u16,
) -> bool {
    sockets.iter().enumerate().any(|(other, socket)| {
        other != index && socket.open && socket.bound && socket.local_port == port
    }) || streams.iter().enumerate().any(|(other, stream)| {
        other != index
            && stream
                .as_ref()
                .is_some_and(|stream| stream.bound() && stream.local_port == port)
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
