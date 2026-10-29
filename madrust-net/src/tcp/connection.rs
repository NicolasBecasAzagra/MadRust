// SPDX-License-Identifier: GPL-2.0-only
//! The states of a TCP connection (RFC 9293, formerly RFC 793).
//!
//! A connection is a state and a pair of sequence numbers. The state is one of
//! the eleven of the specification; the numbers say which byte was sent last,
//! which byte was acknowledged, and which byte the peer will send next. The
//! logic is a function: [`Connection::on`] takes the state and one event and
//! returns the new state with the segments to send, if any. No clock is read:
//! the caller says how much time has passed, and the state decides whether a
//! segment must go again.
//!
//! The events are the opening ([`Event::Open`] for the side that starts,
//! [`Event::Listen`] for the side that waits), the arrival of a segment, the
//! bytes the application hands down, the closing, and the passage of time.
//! Where the specification has two answers to one arrival, the flags of the
//! segment decide: a SYN opens, an ACK acknowledges, a FIN closes one side and
//! a RST takes the connection away from both.
//!
//! One segment is in flight at a time. The state keeps the sequence number, the
//! flags and the length of the last unacknowledged segment, and sends it again
//! when the timer expires: [`Response::Retransmit`] names the bytes the caller
//! must put on the wire again, because the payload lives in the buffer of the
//! caller and not in this crate. After [`MAX_ATTEMPTS`] transmissions without
//! an acknowledgement the connection is gone: the state is [`State::Closed`]
//! and the response is [`Response::Aborted`].
//!
//! The ports come from [`Connection::new`]. The addresses do not: the checksum
//! of a segment is computed when it is written with
//! [`crate::tcp::Header::fill_checksum`], where both addresses are known.
//!
//! The limits are the ones that keep the logic small enough to be read and
//! tested: one segment is in flight at a time, the header carries no option,
//! a closing while a handshake is in progress is ignored, a FIN that arrives
//! behind a gap is not taken because the state keeps no marker of its own for
//! it, and a segment that arrives in [`State::Closed`] is dropped without an
//! answer. Each one is a change of a field or of a branch when a caller needs
//! more.

use super::header::{Flags, Header, Segment};
use super::receiver::{Receiver, BUFFER_SIZE};

/// Transmissions of one segment before the connection is given up.
pub const MAX_ATTEMPTS: u8 = 5;

/// Time the caller counts before the first retransmission.
///
/// The unit is the one the caller passes to [`Event::Tick`]: milliseconds,
/// timer ticks, or the units of the clock of the kernel.
pub const INITIAL_TIMEOUT: u32 = 1000;

/// Time the connection stays in [`State::TimeWait`] before it is closed.
pub const TIME_WAIT_TIMEOUT: u32 = 2000;

/// The eleven states of a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// No connection exists.
    Closed,
    /// A connection waits for a SYN.
    Listen,
    /// A SYN was sent, the answer has not arrived.
    SynSent,
    /// A SYN arrived, the SYN and the ACK were sent, the ACK has not arrived.
    SynReceived,
    /// Both sides may send data.
    Established,
    /// This side sent its FIN and waits for the acknowledgement of it.
    FinWait1,
    /// This side sent its FIN and it was acknowledged; the peer may still send.
    FinWait2,
    /// The peer sent its FIN; this side may still send.
    CloseWait,
    /// Both sides sent a FIN, neither has acknowledged the other yet.
    Closing,
    /// The peer closed and this side sent its FIN; the ACK has not arrived.
    LastAck,
    /// Both sides closed; the connection waits before it forgets the numbers.
    TimeWait,
}

/// One thing that happens to a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// The application starts a connection: a SYN goes out and the state
    /// becomes [`State::SynSent`]. `sequence` is the initial sequence number.
    Open {
        /// Initial sequence number this side chooses.
        sequence: u32,
    },
    /// The application waits for a connection: the state becomes
    /// [`State::Listen`] and nothing goes out. `sequence` is the initial
    /// sequence number the answer will carry.
    Listen {
        /// Initial sequence number this side will use.
        sequence: u32,
    },
    /// A segment arrived.
    Receive(Segment<'a>),
    /// The application hands `payload` down to be sent.
    Send(&'a [u8]),
    /// The application closes: this side has nothing left to send.
    Close,
    /// `elapsed` units of time passed since the last event.
    Tick {
        /// Time that passed, in the unit of [`INITIAL_TIMEOUT`].
        elapsed: u32,
    },
}

/// A segment to put on the wire again.
///
/// The payload is not here: the caller kept the bytes it sent, and
/// `payload_length` of them, starting at `sequence`, are the ones to send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retransmission {
    /// Sequence number of the first byte to send again.
    pub sequence: u32,
    /// Flags of the segment.
    pub flags: Flags,
    /// Bytes of payload to send again after the header.
    pub payload_length: usize,
}

/// What the connection wants the caller to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response<'a> {
    /// Put this segment on the wire.
    Send(Segment<'a>),
    /// Put the unacknowledged bytes on the wire again.
    Retransmit(Retransmission),
    /// The connection is gone: no acknowledgement arrived in time.
    Aborted,
}

/// The state after an event, and the segment to send, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition<'a> {
    /// The state of the connection after the event.
    pub state: State,
    /// The segment to send, or nothing.
    pub response: Option<Response<'a>>,
}

/// A segment this side sent and the peer has not acknowledged yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pending {
    sequence: u32,
    flags: Flags,
    length: usize,
    remaining: u32,
    attempts: u8,
}

/// One connection, its numbers, and the bytes it received in order.
pub struct Connection {
    state: State,
    local_port: u16,
    remote_port: u16,
    local_sequence: u32,
    remote_sequence: u32,
    snd_una: u32,
    snd_nxt: u32,
    snd_wnd: u16,
    receiver: Receiver,
    pending: Option<Pending>,
    sent: u32,
    received: u32,
    time_wait: u32,
}

impl Connection {
    /// A connection that does not exist yet, between two ports.
    pub const fn new(local_port: u16, remote_port: u16) -> Self {
        Self {
            state: State::Closed,
            local_port,
            remote_port,
            local_sequence: 0,
            remote_sequence: 0,
            snd_una: 0,
            snd_nxt: 0,
            snd_wnd: 0,
            receiver: Receiver::new(0, BUFFER_SIZE as u16),
            pending: None,
            sent: 0,
            received: 0,
            time_wait: 0,
        }
    }

    /// The state of the connection.
    pub const fn state(&self) -> State {
        self.state
    }

    /// Port of this side.
    pub const fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Port of the peer.
    pub const fn remote_port(&self) -> u16 {
        self.remote_port
    }

    /// The initial sequence number this side chose.
    pub const fn local_sequence(&self) -> u32 {
        self.local_sequence
    }

    /// The initial sequence number of the peer.
    pub const fn remote_sequence(&self) -> u32 {
        self.remote_sequence
    }

    /// Sequence number of the last byte this side sent, plus one.
    pub const fn send_next(&self) -> u32 {
        self.snd_nxt
    }

    /// Sequence number of the first byte the peer has not acknowledged.
    pub const fn send_unacknowledged(&self) -> u32 {
        self.snd_una
    }

    /// Sequence number of the next byte the peer will send.
    pub const fn receive_next(&self) -> u32 {
        self.receiver.next()
    }

    /// Bytes this side sent as payload.
    pub const fn sent_bytes(&self) -> u32 {
        self.sent
    }

    /// Bytes of the stream this side received in order.
    pub const fn received_bytes(&self) -> u32 {
        self.received
    }

    /// Bytes the peer said it can receive.
    pub const fn send_window(&self) -> u16 {
        self.snd_wnd
    }

    /// Bytes this side is willing to receive.
    pub fn receive_window(&self) -> u16 {
        self.receiver.window()
    }

    /// The bytes of the stream that are in order and not read yet.
    pub fn received(&self) -> &[u8] {
        self.receiver.ready()
    }

    /// Drop the first `count` bytes of the stream, the ones the application
    /// read.
    pub fn consume(&mut self, count: usize) {
        self.receiver.consume(count);
    }

    /// Bytes of payload sent and not acknowledged yet.
    pub fn unacknowledged(&self) -> u32 {
        self.snd_nxt.wrapping_sub(self.snd_una)
    }

    /// Apply one event and return the state with the segment to send.
    pub fn on<'a>(&mut self, event: Event<'a>) -> Transition<'a> {
        let response = match event {
            Event::Open { sequence } => self.open(sequence),
            Event::Listen { sequence } => self.listen(sequence),
            Event::Receive(segment) => self.receive(segment),
            Event::Send(payload) => self.send(payload),
            Event::Close => self.close(),
            Event::Tick { elapsed } => self.tick(elapsed),
        };
        Transition {
            state: self.state,
            response,
        }
    }

    /// A segment of this connection with `flags`, `sequence`, `acknowledgement`
    /// and `payload`.
    fn segment<'a>(
        &self,
        flags: Flags,
        sequence: u32,
        acknowledgement: u32,
        payload: &'a [u8],
    ) -> Segment<'a> {
        Segment {
            header: Header::new(
                self.local_port,
                self.remote_port,
                sequence,
                acknowledgement,
                flags,
                self.receiver.window(),
            ),
            payload,
        }
    }

    /// The application starts a connection.
    fn open<'a>(&mut self, sequence: u32) -> Option<Response<'a>> {
        if self.state != State::Closed {
            return None;
        }
        self.local_sequence = sequence;
        self.snd_una = sequence;
        self.snd_nxt = sequence.wrapping_add(1);
        self.state = State::SynSent;
        self.pending = Some(Pending {
            sequence,
            flags: Flags::SYN,
            length: 0,
            remaining: INITIAL_TIMEOUT,
            attempts: 0,
        });
        let segment = self.segment(Flags::SYN, sequence, 0, &[]);
        Some(Response::Send(segment))
    }

    /// The application waits for a connection.
    fn listen<'a>(&mut self, sequence: u32) -> Option<Response<'a>> {
        if self.state != State::Closed {
            return None;
        }
        self.local_sequence = sequence;
        self.snd_una = sequence;
        self.snd_nxt = sequence.wrapping_add(1);
        self.state = State::Listen;
        None
    }

    /// The application hands bytes down.
    fn send<'a>(&mut self, payload: &'a [u8]) -> Option<Response<'a>> {
        if !matches!(self.state, State::Established | State::CloseWait) {
            return None;
        }
        if payload.is_empty() {
            return None;
        }

        let sequence = self.snd_nxt;
        let flags = Flags::ACK | Flags::PSH;
        let segment = self.segment(flags, sequence, self.receiver.next(), payload);
        self.snd_nxt = self.snd_nxt.wrapping_add(payload.len() as u32);
        self.sent = self.sent.wrapping_add(payload.len() as u32);
        self.pending = Some(Pending {
            sequence,
            flags,
            length: payload.len(),
            remaining: INITIAL_TIMEOUT,
            attempts: 0,
        });
        Some(Response::Send(segment))
    }

    /// The application closes.
    fn close<'a>(&mut self) -> Option<Response<'a>> {
        match self.state {
            State::Established => {
                self.state = State::FinWait1;
                Some(self.finish())
            }
            State::CloseWait => {
                self.state = State::LastAck;
                Some(self.finish())
            }
            _ => None,
        }
    }

    /// Send a FIN and remember it as unacknowledged.
    fn finish<'a>(&mut self) -> Response<'a> {
        let sequence = self.snd_nxt;
        let flags = Flags::ACK | Flags::FIN;
        let segment = self.segment(flags, sequence, self.receiver.next(), &[]);
        self.snd_nxt = sequence.wrapping_add(1);
        self.pending = Some(Pending {
            sequence,
            flags,
            length: 0,
            remaining: INITIAL_TIMEOUT,
            attempts: 0,
        });
        Response::Send(segment)
    }

    /// A segment arrived.
    fn receive<'a>(&mut self, segment: Segment<'a>) -> Option<Response<'a>> {
        let header = segment.header;
        let flags = header.flags;

        // A reset ends every state, whatever the rest of the segment says.
        if flags.contains(Flags::RST) {
            self.pending = None;
            self.state = State::Closed;
            return None;
        }

        match self.state {
            State::Closed => None,
            State::Listen => {
                if flags.contains(Flags::SYN) && !flags.contains(Flags::ACK) {
                    self.remote_sequence = header.sequence;
                    self.receiver.reset(header.sequence.wrapping_add(1));
                    self.snd_una = self.local_sequence;
                    self.snd_nxt = self.local_sequence.wrapping_add(1);
                    self.state = State::SynReceived;
                    let flags = Flags::SYN | Flags::ACK;
                    let segment =
                        self.segment(flags, self.local_sequence, self.receiver.next(), &[]);
                    self.pending = Some(Pending {
                        sequence: self.local_sequence,
                        flags,
                        length: 0,
                        remaining: INITIAL_TIMEOUT,
                        attempts: 0,
                    });
                    Some(Response::Send(segment))
                } else {
                    None
                }
            }
            State::SynSent => {
                if flags.contains(Flags::SYN) && flags.contains(Flags::ACK) {
                    if header.acknowledgement != self.snd_nxt {
                        return None;
                    }
                    self.acknowledge(header.acknowledgement, header.window);
                    self.remote_sequence = header.sequence;
                    self.receiver.reset(header.sequence.wrapping_add(1));
                    self.state = State::Established;
                    let segment = self.segment(Flags::ACK, self.snd_nxt, self.receiver.next(), &[]);
                    Some(Response::Send(segment))
                } else if flags.contains(Flags::SYN) {
                    // Both sides opened at once: answer the SYN as a listener
                    // does and wait for the ACK.
                    self.remote_sequence = header.sequence;
                    self.receiver.reset(header.sequence.wrapping_add(1));
                    self.state = State::SynReceived;
                    let flags = Flags::SYN | Flags::ACK;
                    let segment =
                        self.segment(flags, self.local_sequence, self.receiver.next(), &[]);
                    self.pending = Some(Pending {
                        sequence: self.local_sequence,
                        flags,
                        length: 0,
                        remaining: INITIAL_TIMEOUT,
                        attempts: 0,
                    });
                    Some(Response::Send(segment))
                } else {
                    None
                }
            }
            State::SynReceived => {
                if flags.contains(Flags::SYN) && !flags.contains(Flags::ACK) {
                    // The SYN arrived again: the answer was lost.
                    let flags = Flags::SYN | Flags::ACK;
                    let segment =
                        self.segment(flags, self.local_sequence, self.receiver.next(), &[]);
                    Some(Response::Send(segment))
                } else if flags.contains(Flags::ACK) && header.acknowledgement == self.snd_nxt {
                    self.acknowledge(header.acknowledgement, header.window);
                    self.pending = None;
                    self.state = State::Established;
                    None
                } else {
                    None
                }
            }
            State::Established => self.receive_stream(segment),
            State::FinWait1 => {
                let mut acknowledged = false;
                if flags.contains(Flags::ACK) && header.acknowledgement == self.snd_nxt {
                    self.acknowledge(header.acknowledgement, header.window);
                    self.pending = None;
                    acknowledged = true;
                }
                self.absorb(segment);
                let mut response = None;
                if flags.contains(Flags::FIN) {
                    if acknowledged {
                        self.state = State::TimeWait;
                        self.time_wait = 0;
                    } else {
                        self.state = State::Closing;
                    }
                    self.take_fin(segment);
                    response = Some(self.acknowledge_segment());
                } else if acknowledged {
                    self.state = State::FinWait2;
                }
                response
            }
            State::FinWait2 => {
                if flags.contains(Flags::ACK) {
                    self.acknowledge(header.acknowledgement, header.window);
                }
                self.absorb(segment);
                if flags.contains(Flags::FIN) {
                    self.take_fin(segment);
                    self.state = State::TimeWait;
                    self.time_wait = 0;
                    Some(self.acknowledge_segment())
                } else {
                    None
                }
            }
            State::CloseWait => self.receive_stream(segment),
            State::Closing => {
                let mut response = None;
                if flags.contains(Flags::ACK) && header.acknowledgement == self.snd_nxt {
                    self.acknowledge(header.acknowledgement, header.window);
                    self.pending = None;
                    self.state = State::TimeWait;
                    self.time_wait = 0;
                }
                if flags.contains(Flags::FIN) {
                    self.take_fin(segment);
                    response = Some(self.acknowledge_segment());
                }
                response
            }
            State::LastAck => {
                if flags.contains(Flags::ACK) && header.acknowledgement == self.snd_nxt {
                    self.acknowledge(header.acknowledgement, header.window);
                    self.pending = None;
                    self.state = State::Closed;
                }
                None
            }
            State::TimeWait => {
                if flags.contains(Flags::FIN) {
                    // The last ACK was lost: the peer sends its FIN again.
                    self.time_wait = 0;
                    Some(self.acknowledge_segment())
                } else {
                    None
                }
            }
        }
    }

    /// The payload and the FIN of a segment, in [`State::Established`] and
    /// [`State::CloseWait`].
    fn receive_stream<'a>(&mut self, segment: Segment<'a>) -> Option<Response<'a>> {
        let header = segment.header;
        let flags = header.flags;
        if flags.contains(Flags::ACK) {
            self.acknowledge(header.acknowledgement, header.window);
        }
        let had_bytes = !segment.payload.is_empty();
        self.absorb(segment);
        if flags.contains(Flags::FIN) {
            self.take_fin(segment);
            if self.state == State::Established {
                self.state = State::CloseWait;
            }
            return Some(self.acknowledge_segment());
        }
        if had_bytes {
            Some(self.acknowledge_segment())
        } else {
            None
        }
    }

    /// Take the payload of a segment into the receiver.
    fn absorb(&mut self, segment: Segment<'_>) {
        if segment.payload.is_empty() {
            return;
        }
        let accepted = self
            .receiver
            .accept(segment.header.sequence, segment.payload);
        self.received = self.received.wrapping_add(accepted.appended as u32);
    }

    /// Take the sequence number of the FIN of a segment when the bytes before
    /// it are in order.
    fn take_fin(&mut self, segment: Segment<'_>) {
        let end = segment
            .header
            .sequence
            .wrapping_add(segment.payload.len() as u32);
        if self.receiver.next() == end {
            self.receiver.skip(1);
        }
    }

    /// A segment that acknowledges everything received so far.
    fn acknowledge_segment<'a>(&self) -> Response<'a> {
        Response::Send(self.segment(Flags::ACK, self.snd_nxt, self.receiver.next(), &[]))
    }

    /// Take an acknowledgement if it names bytes this side sent.
    fn acknowledge(&mut self, acknowledgement: u32, window: u16) {
        let unacknowledged = self.unacknowledged();
        let advanced = acknowledgement.wrapping_sub(self.snd_una);
        if advanced == 0 || advanced > unacknowledged {
            return;
        }
        self.snd_wnd = window;
        self.snd_una = acknowledgement;

        let covered = advanced as usize;
        let clear = match &mut self.pending {
            Some(pending) if covered >= pending.length => true,
            Some(pending) => {
                pending.sequence = pending.sequence.wrapping_add(advanced);
                pending.length -= covered;
                pending.remaining = INITIAL_TIMEOUT;
                pending.attempts = 0;
                false
            }
            None => false,
        };
        if clear {
            self.pending = None;
        }
    }

    /// Time passed.
    fn tick<'a>(&mut self, elapsed: u32) -> Option<Response<'a>> {
        if self.state == State::TimeWait {
            self.time_wait = self.time_wait.saturating_add(elapsed);
            if self.time_wait >= TIME_WAIT_TIMEOUT {
                self.state = State::Closed;
            }
            return None;
        }

        let pending = self.pending.as_mut()?;
        if elapsed < pending.remaining {
            pending.remaining -= elapsed;
            return None;
        }

        pending.attempts = pending.attempts.saturating_add(1);
        if pending.attempts >= MAX_ATTEMPTS {
            self.pending = None;
            self.state = State::Closed;
            return Some(Response::Aborted);
        }

        // Double the wait after every attempt, up to four doublings.
        let doublings = pending.attempts.min(4);
        pending.remaining = INITIAL_TIMEOUT << doublings;
        let retransmission = Retransmission {
            sequence: pending.sequence,
            flags: pending.flags,
            payload_length: pending.length,
        };
        Some(Response::Retransmit(retransmission))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Connection {
        Connection::new(1234, 80)
    }

    fn server() -> Connection {
        Connection::new(80, 1234)
    }

    /// The segment a transition wants to send.
    fn sent<'a>(transition: Transition<'a>) -> Segment<'a> {
        match transition.response {
            Some(Response::Send(segment)) => segment,
            other => panic!("expected a segment, got {other:?}"),
        }
    }

    /// A segment from the port of the server to the port of the client.
    fn from_server<'a>(
        sequence: u32,
        acknowledgement: u32,
        flags: Flags,
        payload: &'a [u8],
    ) -> Segment<'a> {
        Segment {
            header: Header::new(80, 1234, sequence, acknowledgement, flags, 1024),
            payload,
        }
    }

    /// A segment from the port of the client to the port of the server.
    fn from_client<'a>(
        sequence: u32,
        acknowledgement: u32,
        flags: Flags,
        payload: &'a [u8],
    ) -> Segment<'a> {
        Segment {
            header: Header::new(1234, 80, sequence, acknowledgement, flags, 1024),
            payload,
        }
    }

    /// A pair of connections that finished the handshake, with the initial
    /// sequence number 100 on one side and 900 on the other.
    fn established() -> (Connection, Connection) {
        let mut client = client();
        let mut server = server();
        let syn = sent(client.on(Event::Open { sequence: 100 }));
        assert_eq!(
            server.on(Event::Listen { sequence: 900 }).state,
            State::Listen
        );
        let syn_ack = sent(server.on(Event::Receive(syn)));
        let ack = sent(client.on(Event::Receive(syn_ack)));
        assert_eq!(server.on(Event::Receive(ack)).state, State::Established);
        (client, server)
    }

    #[test]
    fn the_handshake_is_the_three_segments_of_the_specification() {
        let mut client = client();
        let mut server = server();

        let opening = client.on(Event::Open { sequence: 1000 });
        assert_eq!(opening.state, State::SynSent);
        let syn = sent(opening);
        assert_eq!(syn.header.source_port, 1234);
        assert_eq!(syn.header.destination_port, 80);
        assert_eq!(syn.header.flags, Flags::SYN);
        assert_eq!(syn.header.sequence, 1000);
        assert_eq!(syn.header.acknowledgement, 0);
        assert!(syn.payload.is_empty());

        assert_eq!(
            server.on(Event::Listen { sequence: 5000 }).state,
            State::Listen
        );

        let answering = server.on(Event::Receive(syn));
        assert_eq!(answering.state, State::SynReceived);
        let syn_ack = sent(answering);
        assert_eq!(syn_ack.header.source_port, 80);
        assert_eq!(syn_ack.header.destination_port, 1234);
        assert_eq!(syn_ack.header.flags, Flags::SYN | Flags::ACK);
        assert_eq!(syn_ack.header.sequence, 5000);
        assert_eq!(syn_ack.header.acknowledgement, 1001);

        let opened = client.on(Event::Receive(syn_ack));
        assert_eq!(opened.state, State::Established);
        let ack = sent(opened);
        assert_eq!(ack.header.flags, Flags::ACK);
        assert_eq!(ack.header.sequence, 1001);
        assert_eq!(ack.header.acknowledgement, 5001);

        let done = server.on(Event::Receive(ack));
        assert_eq!(done.state, State::Established);
        assert!(done.response.is_none());

        assert_eq!(client.local_sequence(), 1000);
        assert_eq!(client.remote_sequence(), 5000);
        assert_eq!(client.send_next(), 1001);
        assert_eq!(client.receive_next(), 5001);
    }

    #[test]
    fn a_connection_goes_through_the_states_of_the_specification() {
        let mut client = client();
        let mut server = server();

        let syn = sent(client.on(Event::Open { sequence: 100 }));
        assert_eq!(client.state(), State::SynSent);
        assert_eq!(
            server.on(Event::Listen { sequence: 900 }).state,
            State::Listen
        );
        let syn_ack = sent(server.on(Event::Receive(syn)));
        let ack = sent(client.on(Event::Receive(syn_ack)));
        assert_eq!(client.state(), State::Established);
        assert_eq!(server.on(Event::Receive(ack)).state, State::Established);

        // The client sends three bytes; the server acknowledges them.
        let data = sent(client.on(Event::Send(b"GET")));
        assert_eq!(data.header.flags, Flags::ACK | Flags::PSH);
        assert_eq!(data.header.sequence, 101);
        assert_eq!(data.header.acknowledgement, 901);
        assert_eq!(data.payload, b"GET");
        assert_eq!(client.send_next(), 104);
        assert_eq!(client.sent_bytes(), 3);

        let ack = sent(server.on(Event::Receive(data)));
        assert_eq!(server.state(), State::Established);
        assert_eq!(ack.header.flags, Flags::ACK);
        assert_eq!(ack.header.sequence, 901);
        assert_eq!(ack.header.acknowledgement, 104);
        assert_eq!(server.received(), b"GET");
        assert_eq!(server.received_bytes(), 3);
        assert_eq!(server.receive_next(), 104);

        let acknowledged = client.on(Event::Receive(ack));
        assert_eq!(acknowledged.state, State::Established);
        assert!(acknowledged.response.is_none());
        assert_eq!(client.send_unacknowledged(), 104);
        // The server read nothing, so its window is three bytes smaller.
        assert_eq!(client.send_window(), 1021);

        // The client closes; the server acknowledges the FIN.
        let fin = sent(client.on(Event::Close));
        assert_eq!(client.state(), State::FinWait1);
        assert_eq!(fin.header.flags, Flags::ACK | Flags::FIN);
        assert_eq!(fin.header.sequence, 104);
        assert_eq!(fin.header.acknowledgement, 901);

        let ack = sent(server.on(Event::Receive(fin)));
        assert_eq!(server.state(), State::CloseWait);
        assert_eq!(ack.header.sequence, 901);
        assert_eq!(ack.header.acknowledgement, 105);

        let acknowledged = client.on(Event::Receive(ack));
        assert_eq!(acknowledged.state, State::FinWait2);
        assert_eq!(client.send_next(), 105);

        // The server closes too, and the client acknowledges its FIN.
        let fin = sent(server.on(Event::Close));
        assert_eq!(server.state(), State::LastAck);
        assert_eq!(fin.header.flags, Flags::ACK | Flags::FIN);
        assert_eq!(fin.header.sequence, 901);
        assert_eq!(fin.header.acknowledgement, 105);

        let ack = sent(client.on(Event::Receive(fin)));
        assert_eq!(client.state(), State::TimeWait);
        assert_eq!(ack.header.flags, Flags::ACK);
        assert_eq!(ack.header.sequence, 105);
        assert_eq!(ack.header.acknowledgement, 902);

        let closed = server.on(Event::Receive(ack));
        assert_eq!(closed.state, State::Closed);
        assert!(closed.response.is_none());

        // The last ACK was lost: the FIN arrives again and is answered.
        let repeated = client.on(Event::Receive(from_server(
            901,
            105,
            Flags::FIN | Flags::ACK,
            &[],
        )));
        assert_eq!(repeated.state, State::TimeWait);
        assert_eq!(sent(repeated).header.flags, Flags::ACK);

        assert_eq!(
            client
                .on(Event::Tick {
                    elapsed: TIME_WAIT_TIMEOUT
                })
                .state,
            State::Closed
        );
    }

    #[test]
    fn two_sides_that_close_at_once_go_through_closing() {
        let (mut client, mut server) = established();

        let client_fin = sent(client.on(Event::Close));
        assert_eq!(client_fin.header.sequence, 101);
        assert_eq!(client_fin.header.acknowledgement, 901);
        assert_eq!(client.state(), State::FinWait1);

        // The server closes before it has seen the FIN of the client.
        let server_fin = sent(server.on(Event::Close));
        assert_eq!(server_fin.header.sequence, 901);
        assert_eq!(server_fin.header.acknowledgement, 101);
        assert_eq!(server.state(), State::FinWait1);

        let closing = client.on(Event::Receive(server_fin));
        assert_eq!(closing.state, State::Closing);
        let client_ack = sent(closing);
        assert_eq!(client_ack.header.sequence, 102);
        assert_eq!(client_ack.header.acknowledgement, 902);

        let closing = server.on(Event::Receive(client_fin));
        assert_eq!(closing.state, State::Closing);
        let server_ack = sent(closing);
        assert_eq!(server_ack.header.sequence, 902);
        assert_eq!(server_ack.header.acknowledgement, 102);

        assert_eq!(client.on(Event::Receive(server_ack)).state, State::TimeWait);
        assert_eq!(server.on(Event::Receive(client_ack)).state, State::TimeWait);
    }

    #[test]
    fn a_reset_closes_the_connection() {
        let (mut client, _) = established();

        let reset = client.on(Event::Receive(from_server(901, 101, Flags::RST, &[])));

        assert_eq!(reset.state, State::Closed);
        assert!(reset.response.is_none());
    }

    #[test]
    fn a_segment_that_arrives_twice_does_not_duplicate_the_bytes() {
        let (_, mut server) = established();
        let data = from_client(101, 901, Flags::ACK | Flags::PSH, b"hello");

        let first = server.on(Event::Receive(data));
        assert_eq!(first.state, State::Established);
        assert_eq!(sent(first).header.acknowledgement, 106);
        assert_eq!(server.received(), b"hello");

        let second = server.on(Event::Receive(data));
        assert_eq!(second.state, State::Established);
        assert_eq!(sent(second).header.acknowledgement, 106);
        assert_eq!(server.received(), b"hello");
        assert_eq!(server.received_bytes(), 5);
        assert_eq!(server.receive_next(), 106);
    }

    #[test]
    fn a_segment_out_of_order_is_held_until_the_one_before_it_arrives() {
        let (_, mut server) = established();

        let held = server.on(Event::Receive(from_client(
            106,
            901,
            Flags::ACK | Flags::PSH,
            b"world",
        )));
        assert_eq!(held.state, State::Established);
        assert_eq!(sent(held).header.acknowledgement, 101);
        assert_eq!(server.received(), b"");
        assert_eq!(server.received_bytes(), 0);

        let joined = server.on(Event::Receive(from_client(
            101,
            901,
            Flags::ACK | Flags::PSH,
            b"hello",
        )));
        assert_eq!(joined.state, State::Established);
        assert_eq!(sent(joined).header.acknowledgement, 111);
        assert_eq!(server.received(), b"helloworld");
        assert_eq!(server.received_bytes(), 10);
        assert_eq!(server.receive_next(), 111);
    }

    #[test]
    fn an_unacknowledged_segment_is_sent_again_and_then_the_connection_is_gone() {
        let mut client = client();
        let syn = sent(client.on(Event::Open { sequence: 1000 }));

        let waiting = client.on(Event::Tick {
            elapsed: INITIAL_TIMEOUT - 1,
        });
        assert_eq!(waiting.state, State::SynSent);
        assert!(waiting.response.is_none());

        let mut retransmissions = 0;
        for _ in 0..100 {
            let transition = client.on(Event::Tick {
                elapsed: INITIAL_TIMEOUT,
            });
            match transition.response {
                Some(Response::Retransmit(again)) => {
                    assert_eq!(again.sequence, syn.header.sequence);
                    assert_eq!(again.flags, Flags::SYN);
                    assert_eq!(again.payload_length, 0);
                    retransmissions += 1;
                }
                Some(Response::Aborted) => {
                    assert_eq!(transition.state, State::Closed);
                    break;
                }
                // The wait doubled, so this tick did not reach the timeout.
                None => {}
                other => panic!("expected a retransmission or an abort, got {other:?}"),
            }
        }

        assert_eq!(retransmissions, usize::from(MAX_ATTEMPTS) - 1);
        assert_eq!(client.state(), State::Closed);
    }

    #[test]
    fn an_unacknowledged_payload_is_sent_again_from_the_first_missing_byte() {
        let (mut client, _) = established();
        let data = sent(client.on(Event::Send(b"GET")));

        let retransmission = client.on(Event::Tick {
            elapsed: INITIAL_TIMEOUT,
        });
        match retransmission.response {
            Some(Response::Retransmit(again)) => {
                assert_eq!(again.sequence, data.header.sequence);
                assert_eq!(again.flags, Flags::ACK | Flags::PSH);
                assert_eq!(again.payload_length, 3);
            }
            other => panic!("expected a retransmission, got {other:?}"),
        }

        // One byte is acknowledged; two remain to send again.
        let partial = client.on(Event::Receive(from_server(901, 102, Flags::ACK, &[])));
        assert_eq!(partial.state, State::Established);
        assert_eq!(client.unacknowledged(), 2);

        let retransmission = client.on(Event::Tick {
            elapsed: INITIAL_TIMEOUT,
        });
        match retransmission.response {
            Some(Response::Retransmit(again)) => {
                assert_eq!(again.sequence, 102);
                assert_eq!(again.payload_length, 2);
            }
            other => panic!("expected a retransmission, got {other:?}"),
        }
    }

    #[test]
    fn a_retransmitted_syn_is_answered_again() {
        let mut client = client();
        let mut server = server();
        let syn = sent(client.on(Event::Open { sequence: 100 }));
        assert_eq!(
            server.on(Event::Listen { sequence: 900 }).state,
            State::Listen
        );
        let first = sent(server.on(Event::Receive(syn)));
        assert_eq!(first.header.flags, Flags::SYN | Flags::ACK);

        let second = sent(server.on(Event::Receive(syn)));

        assert_eq!(second.header.flags, Flags::SYN | Flags::ACK);
        assert_eq!(second.header.sequence, 900);
        assert_eq!(second.header.acknowledgement, 101);
    }

    #[test]
    fn an_event_that_does_not_apply_changes_nothing() {
        let mut client = client();

        let transition = client.on(Event::Send(b"too early"));

        assert_eq!(transition.state, State::Closed);
        assert!(transition.response.is_none());
        assert_eq!(client.sent_bytes(), 0);
    }

    #[test]
    fn the_receive_window_shrinks_with_the_bytes_not_read() {
        let (_, mut server) = established();

        server.on(Event::Receive(from_client(
            101,
            901,
            Flags::ACK | Flags::PSH,
            b"hello",
        )));
        assert_eq!(server.receive_window(), BUFFER_SIZE as u16 - 5);

        server.consume(5);
        assert_eq!(server.receive_window(), BUFFER_SIZE as u16);
    }
}
