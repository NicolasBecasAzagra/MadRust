// SPDX-License-Identifier: GPL-2.0-only
//! The bytes a receiver holds, in order and out of order.
//!
//! A TCP receiver names the sequence number of the next byte it expects, so a
//! segment that arrives after a gap cannot be handed to the application yet.
//! [`Receiver`] keeps the bytes that continue the stream in its buffer, keeps a
//! few segments that follow a gap in fixed slots, and joins them when the gap
//! is filled. A segment whose bytes were already taken is dropped, so a
//! duplicate never enters the stream twice.
//!
//! Everything is a fixed array: no allocation, no clock and no hardware. A
//! segment that carries more than [`MAX_HELD_PAYLOAD`] bytes behind a gap, or
//! that finds every slot full, is refused, and the sender will send it again.
//! The buffer of [`BUFFER_SIZE`] bytes is also the window [`Receiver::window`]
//! advertises: the bytes held in order are the bytes the application has not
//! read yet.

use super::sequence_distance;

/// Bytes the receiver keeps, in order, before the application reads them.
pub const BUFFER_SIZE: usize = 1024;

/// Segments the receiver keeps behind a gap.
pub const MAX_HELD: usize = 4;

/// Largest payload of a segment held behind a gap.
pub const MAX_HELD_PAYLOAD: usize = 256;

/// What became of the bytes of a segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The bytes continue the stream and are ready to read.
    InOrder,
    /// The bytes follow a gap and are held until the gap is filled.
    Held,
    /// Every byte was already received, so nothing was added.
    Duplicate,
    /// The segment carries no byte, so there is nothing to receive.
    Empty,
    /// The bytes do not fit the window or the slots of the receiver.
    Refused,
}

/// The bytes of a segment the receiver added, and what became of the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accepted {
    /// What became of the segment.
    pub outcome: Outcome,
    /// Bytes added to the stream, the ones joined when a gap closed included.
    pub appended: usize,
}

/// A segment waiting behind a gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    sequence: u32,
    length: usize,
    bytes: [u8; MAX_HELD_PAYLOAD],
}

/// The bytes of one direction of a connection, in the order of the stream.
pub struct Receiver {
    next: u32,
    capacity: usize,
    length: usize,
    bytes: [u8; BUFFER_SIZE],
    held: [Option<Held>; MAX_HELD],
}

impl Receiver {
    /// A receiver that expects `next` as its first byte and can hold `window`
    /// bytes, at most [`BUFFER_SIZE`].
    pub const fn new(next: u32, window: u16) -> Self {
        let capacity = if window as usize > BUFFER_SIZE {
            BUFFER_SIZE
        } else {
            window as usize
        };
        Self {
            next,
            capacity,
            length: 0,
            bytes: [0u8; BUFFER_SIZE],
            held: [None; MAX_HELD],
        }
    }

    /// Expect `next` as the first byte of a new stream.
    ///
    /// The bytes of the old stream are dropped: this is what the arrival of a
    /// SYN does to a receiver.
    pub fn reset(&mut self, next: u32) {
        self.next = next;
        self.length = 0;
        self.held = [None; MAX_HELD];
    }

    /// Sequence number of the next byte the stream expects.
    pub const fn next(&self) -> u32 {
        self.next
    }

    /// Bytes the receiver can hold before the application reads them.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Bytes the sender may still put on the wire: the room left.
    pub fn window(&self) -> u16 {
        (self.capacity - self.length) as u16
    }

    /// The bytes of the stream that are in order and not read yet.
    pub fn ready(&self) -> &[u8] {
        &self.bytes[..self.length]
    }

    /// How many bytes of the stream are in order and not read yet.
    pub const fn ready_len(&self) -> usize {
        self.length
    }

    /// Drop the first `count` bytes of the stream, the ones the application
    /// has read.
    pub fn consume(&mut self, count: usize) {
        let count = count.min(self.length);
        self.bytes.copy_within(count..self.length, 0);
        self.length -= count;
    }

    /// Advance the sequence number by `count` without adding bytes.
    ///
    /// The FIN of a segment takes one sequence number of its own, so the
    /// receiver skips it once the payload before it is in order.
    pub fn skip(&mut self, count: u32) {
        self.next = self.next.wrapping_add(count);
    }

    /// Take the payload of a segment, in order or behind a gap.
    ///
    /// The bytes that start before [`Receiver::next`] are cut off, so a
    /// repeated segment adds nothing; the bytes that start after it are held
    /// in a slot until the gap is filled; the bytes that start there are
    /// appended and the slots that became contiguous are joined after them.
    /// `appended` is the number of bytes the stream grew by.
    pub fn accept(&mut self, sequence: u32, payload: &[u8]) -> Accepted {
        if payload.is_empty() {
            return Accepted {
                outcome: Outcome::Empty,
                appended: 0,
            };
        }

        let distance = sequence_distance(self.next, sequence);
        let mut bytes = payload;
        if distance < 0 {
            let skip = distance.unsigned_abs() as usize;
            if skip >= bytes.len() {
                return Accepted {
                    outcome: Outcome::Duplicate,
                    appended: 0,
                };
            }
            bytes = &bytes[skip..];
        }

        if distance > 0 {
            return self.hold(sequence, bytes);
        }

        let room = self.capacity - self.length;
        if room == 0 {
            return Accepted {
                outcome: Outcome::Refused,
                appended: 0,
            };
        }
        let take = bytes.len().min(room);
        self.bytes[self.length..self.length + take].copy_from_slice(&bytes[..take]);
        self.length += take;
        self.next = self.next.wrapping_add(take as u32);

        let mut appended = take;
        while let Some(index) = self
            .held
            .iter()
            .position(|held| matches!(held, Some(held) if held.sequence == self.next))
        {
            let Some(held) = self.held[index].take() else {
                break;
            };
            let room = self.capacity - self.length;
            let take = held.length.min(room);
            self.bytes[self.length..self.length + take].copy_from_slice(&held.bytes[..take]);
            self.length += take;
            self.next = self.next.wrapping_add(take as u32);
            appended += take;
            if take < held.length {
                // The window closed before the held bytes could be joined;
                // the sender will send the rest again.
                break;
            }
        }
        self.drop_covered();

        Accepted {
            outcome: Outcome::InOrder,
            appended,
        }
    }

    /// Drop the held segments whose bytes are behind [`Receiver::next`].
    fn drop_covered(&mut self) {
        for slot in &mut self.held {
            let Some(held) = slot else { continue };
            let end = held.sequence.wrapping_add(held.length as u32);
            if sequence_distance(self.next, end) >= 0 {
                *slot = None;
            }
        }
    }

    /// Keep the bytes of a segment that follows a gap.
    fn hold(&mut self, sequence: u32, bytes: &[u8]) -> Accepted {
        if bytes.len() > MAX_HELD_PAYLOAD {
            return Accepted {
                outcome: Outcome::Refused,
                appended: 0,
            };
        }

        // A held segment that shares a byte with another held one would join
        // the same bytes twice, so the newcomer is dropped instead: the
        // duplicate is refused, the partial overlap is left to the sender,
        // which sends it again when the gap closes.
        for held in self.held.iter().flatten() {
            let held_end = held.sequence.wrapping_add(held.length as u32);
            let end = sequence.wrapping_add(bytes.len() as u32);
            let overlaps = sequence_distance(held_end, sequence) < 0
                && sequence_distance(end, held.sequence) < 0;
            if overlaps {
                let covered = sequence_distance(held.sequence, sequence) >= 0
                    && sequence_distance(end, held_end) >= 0;
                return Accepted {
                    outcome: if covered {
                        Outcome::Duplicate
                    } else {
                        Outcome::Refused
                    },
                    appended: 0,
                };
            }
        }

        let Some(index) = self.held.iter().position(Option::is_none) else {
            return Accepted {
                outcome: Outcome::Refused,
                appended: 0,
            };
        };

        let mut held = Held {
            sequence,
            length: bytes.len(),
            bytes: [0u8; MAX_HELD_PAYLOAD],
        };
        held.bytes[..bytes.len()].copy_from_slice(bytes);
        self.held[index] = Some(held);
        Accepted {
            outcome: Outcome::Held,
            appended: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A receiver that expects the byte 1000 and holds 32 bytes.
    fn receiver() -> Receiver {
        Receiver::new(1000, 32)
    }

    #[test]
    fn the_bytes_in_order_are_ready_and_advance_the_sequence() {
        let mut receiver = receiver();

        let accepted = receiver.accept(1000, b"hello");

        assert_eq!(
            accepted,
            Accepted {
                outcome: Outcome::InOrder,
                appended: 5,
            }
        );
        assert_eq!(receiver.ready(), b"hello");
        assert_eq!(receiver.ready_len(), 5);
        assert_eq!(receiver.next(), 1005);
        assert_eq!(receiver.window(), 27);
    }

    #[test]
    fn the_bytes_behind_a_gap_wait_for_the_gap() {
        let mut receiver = receiver();

        let held = receiver.accept(1005, b"world");
        assert_eq!(held.outcome, Outcome::Held);
        assert_eq!(receiver.ready(), b"");
        assert_eq!(receiver.next(), 1000);

        let accepted = receiver.accept(1000, b"hello");
        assert_eq!(accepted.outcome, Outcome::InOrder);
        assert_eq!(accepted.appended, 10);
        assert_eq!(receiver.ready(), b"helloworld");
        assert_eq!(receiver.next(), 1010);
    }

    #[test]
    fn a_segment_that_arrives_twice_adds_nothing() {
        let mut receiver = receiver();
        receiver.accept(1000, b"hello");

        let duplicate = receiver.accept(1000, b"hello");

        assert_eq!(
            duplicate,
            Accepted {
                outcome: Outcome::Duplicate,
                appended: 0,
            }
        );
        assert_eq!(receiver.ready(), b"hello");
        assert_eq!(receiver.next(), 1005);
    }

    #[test]
    fn a_segment_behind_the_sequence_is_cut_at_the_first_new_byte() {
        let mut receiver = receiver();
        receiver.accept(1000, b"hello");

        let accepted = receiver.accept(1003, b"lo world");

        assert_eq!(accepted.outcome, Outcome::InOrder);
        assert_eq!(accepted.appended, 6);
        assert_eq!(receiver.ready(), b"hello world");
        assert_eq!(receiver.next(), 1011);
    }

    #[test]
    fn a_held_segment_that_arrives_twice_is_dropped() {
        let mut receiver = receiver();
        receiver.accept(1005, b"world");

        let duplicate = receiver.accept(1005, b"world");

        assert_eq!(duplicate.outcome, Outcome::Duplicate);
        assert_eq!(duplicate.appended, 0);
    }

    #[test]
    fn a_held_segment_that_overlaps_another_is_not_kept_twice() {
        let mut receiver = receiver();
        assert_eq!(receiver.accept(1005, b"world").outcome, Outcome::Held);

        let inside = receiver.accept(1006, b"or");
        assert_eq!(inside.outcome, Outcome::Duplicate);

        let crossing = receiver.accept(1008, b"ld!!!");
        assert_eq!(crossing.outcome, Outcome::Refused);

        assert_eq!(receiver.accept(1000, b"hello").outcome, Outcome::InOrder);
        assert_eq!(receiver.ready(), b"helloworld");
    }

    #[test]
    fn consuming_bytes_opens_the_window() {
        let mut receiver = receiver();
        receiver.accept(1000, b"hello");

        receiver.consume(2);

        assert_eq!(receiver.ready(), b"llo");
        assert_eq!(receiver.window(), 29);
        assert_eq!(receiver.next(), 1005);

        receiver.consume(100);
        assert_eq!(receiver.ready(), b"");
        assert_eq!(receiver.window(), 32);
    }

    #[test]
    fn an_empty_segment_adds_nothing() {
        let mut receiver = receiver();

        assert_eq!(receiver.accept(1000, &[]).outcome, Outcome::Empty);
        assert_eq!(receiver.ready(), b"");
    }

    #[test]
    fn a_held_segment_larger_than_a_slot_is_refused() {
        let mut receiver = receiver();

        assert_eq!(
            receiver.accept(2000, &[0xaa; MAX_HELD_PAYLOAD + 1]).outcome,
            Outcome::Refused
        );
        assert_eq!(receiver.ready(), b"");
    }

    #[test]
    fn a_held_segment_without_a_free_slot_is_refused() {
        let mut receiver = receiver();
        for index in 0..MAX_HELD {
            let sequence = 2000 + index as u32 * 10;
            assert_eq!(
                receiver.accept(sequence, b"0123456789").outcome,
                Outcome::Held
            );
        }

        assert_eq!(
            receiver.accept(3000, b"0123456789").outcome,
            Outcome::Refused
        );
    }

    #[test]
    fn the_window_that_is_full_refuses_new_bytes() {
        let mut receiver = receiver();
        assert_eq!(receiver.accept(1000, &[0xaa; 32]).outcome, Outcome::InOrder);

        assert_eq!(receiver.accept(1032, &[0xbb; 4]).outcome, Outcome::Refused);
        assert_eq!(receiver.ready_len(), 32);
    }

    #[test]
    fn skipping_a_sequence_number_moves_the_expected_byte() {
        let mut receiver = receiver();
        receiver.accept(1000, b"hello");

        receiver.skip(1);

        assert_eq!(receiver.next(), 1006);
    }

    #[test]
    fn a_window_larger_than_the_buffer_is_the_buffer() {
        let receiver = Receiver::new(0, u16::MAX);

        assert_eq!(receiver.capacity(), BUFFER_SIZE);
    }
}
