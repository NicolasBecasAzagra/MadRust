// SPDX-License-Identifier: GPL-2.0-only
//! TCP: the header of a segment, the states of a connection and the bytes of
//! the stream (RFC 9293, formerly RFC 793).
//!
//! The three pieces are separate so each one is a plain function of plain
//! data:
//!
//! - [`header`] builds and takes apart a segment, and computes the checksum of
//!   a pseudo header, a header and a payload.
//! - [`connection`] holds the eleven states of the specification and moves
//!   between them on the events of an opening, an arrival, a closing and the
//!   passage of time. It returns the segment to send, and it never reads a
//!   clock: the caller says how much time has passed.
//! - [`receiver`] keeps the bytes of the stream in order, holds the segments
//!   that arrive out of order, and drops the ones that arrive twice.
//!
//! The types are re-exported here, so a caller writes [`Header`] and
//! [`Connection`] without naming the module of each one.
//!
//! The connection handles one segment in flight at a time, and it models no
//! option of the header: no maximum segment size, no window scale and no
//! selective acknowledgement. Those are the limits that keep the logic small
//! enough to be tested on the host, and each one is a change of a single
//! field when the kernel needs it.

pub mod connection;
pub mod header;
pub mod receiver;

pub use connection::{
    Connection, Event, Response, Retransmission, State, Transition, INITIAL_TIMEOUT, MAX_ATTEMPTS,
    TIME_WAIT_TIMEOUT,
};
pub use header::{Flags, Header, Segment};
pub use receiver::{Accepted, Outcome, Receiver, BUFFER_SIZE, MAX_HELD, MAX_HELD_PAYLOAD};

/// Distance from `base` to `sequence` in the sequence space, as a signed
/// number of bytes.
///
/// Sequence numbers wrap at 2^32, so a plain subtraction lies across the wrap.
/// The difference of the two's complement values is the distance while both
/// numbers are less than 2^31 apart, which is the window of a connection:
/// negative means `sequence` is behind `base`, positive means it is ahead.
pub(crate) fn sequence_distance(base: u32, sequence: u32) -> i32 {
    sequence.wrapping_sub(base) as i32
}
