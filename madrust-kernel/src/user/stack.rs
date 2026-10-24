// SPDX-License-Identifier: GPL-2.0-only
//! Initial user stack: the System V argument block.
//!
//! The block is built from the top of the stack downwards, in the order the
//! program expects to read it: `argc`, `argv`, `envp`, then the auxiliary
//! vector terminated by `AT_NULL`. The strings and the sixteen random bytes
//! live at the high end and are referenced through pointers.
//!
//! The final stack pointer is 16 byte aligned, as the System V ABI requires at
//! the first instruction of a program. The block is written through the stack
//! virtual addresses: the pages are already mapped in the live page tables and
//! the kernel runs with CPL 0, so supervisor access to them is allowed.

use core::ptr::{copy_nonoverlapping, write_volatile};

use super::loader::Image;
use super::paging;

/// Address one past the last byte of the user stack.
pub const STACK_TOP: u64 = 0x0000_0000_8000_0000;

/// Pages reserved for the user stack, 256 KiB.
///
/// A standard library program builds its startup state and its `stdio`
/// buffers on this stack, so the four pages of the freestanding milestone are
/// not enough: the first `println!` would run off the bottom of the mapping.
pub const STACK_PAGES: u64 = 64;

/// Number of bytes of random data `AT_RANDOM` points at.
const RANDOM_SIZE: usize = 16;

/// Auxiliary vector entry: address of the program header table.
const AT_PHDR: u64 = 3;
/// Auxiliary vector entry: size of one program header.
const AT_PHENT: u64 = 4;
/// Auxiliary vector entry: number of program headers.
const AT_PHNUM: u64 = 5;
/// Auxiliary vector entry: page size.
const AT_PAGESZ: u64 = 6;
/// Auxiliary vector entry: program entry point.
const AT_ENTRY: u64 = 9;
/// Auxiliary vector entry: user id.
const AT_UID: u64 = 11;
/// Auxiliary vector entry: effective user id.
const AT_EUID: u64 = 12;
/// Auxiliary vector entry: group id.
const AT_GID: u64 = 13;
/// Auxiliary vector entry: effective group id.
const AT_EGID: u64 = 14;
/// Auxiliary vector entry: setuid state, zero for a normal program.
const AT_SECURE: u64 = 23;
/// Auxiliary vector entry: address of the random bytes.
const AT_RANDOM: u64 = 25;
/// Auxiliary vector entry: address of the base of the interpreter, zero here.
const AT_BASE: u64 = 7;
/// Auxiliary vector entry: clock ticks per second.
const AT_CLKTCK: u64 = 17;
/// Auxiliary vector entry: end of the vector.
const AT_NULL: u64 = 0;

/// Clock ticks per second reported to the program.
const CLOCK_TICKS_PER_SECOND: u64 = 100;

/// Name the program sees as `argv[0]`.
const PROGRAM_NAME: &[u8] = b"madrust-userland";

/// Reason why the stack could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackError {
    /// A stack page or a page table page could not be mapped.
    OutOfMemory,
}

/// Build the initial stack for `image` and return the stack pointer.
pub fn build(image: &Image, random: [u8; RANDOM_SIZE]) -> Result<u64, StackError> {
    paging::map_range(STACK_TOP, STACK_PAGES).ok_or(StackError::OutOfMemory)?;
    Ok(write_block(image, random))
}

/// Fill the mapped stack pages with the argument block and return the pointer.
fn write_block(image: &Image, random: [u8; RANDOM_SIZE]) -> u64 {
    let mut cursor = STACK_TOP - RANDOM_SIZE as u64;

    // The random bytes first: they stay at a fixed, 16 byte aligned address.
    write_bytes(cursor, &random);
    let random_address = cursor;

    // The program name string, NUL terminated.
    cursor -= PROGRAM_NAME.len() as u64 + 1;
    write_bytes(cursor, PROGRAM_NAME);
    write_bytes(cursor + PROGRAM_NAME.len() as u64, &[0]);
    let name_address = cursor;

    // The auxiliary vector starts on a 16 byte boundary.
    cursor &= !0xf;

    let auxiliary = [
        (AT_PHDR, image.program_headers()),
        (AT_PHENT, u64::from(image.program_header_size())),
        (AT_PHNUM, u64::from(image.program_header_entries())),
        (AT_ENTRY, image.entry()),
        (AT_BASE, 0),
        (AT_PAGESZ, madrust_mm::paging::PAGE_SIZE),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_SECURE, 0),
        (AT_CLKTCK, CLOCK_TICKS_PER_SECOND),
        (AT_RANDOM, random_address),
        (AT_NULL, 0),
    ];
    for (kind, value) in auxiliary.iter().rev() {
        push(&mut cursor, *value);
        push(&mut cursor, *kind);
    }

    // The vector is 14 pairs, 224 bytes, so with the four words below it the
    // whole block is 256 bytes: the final pointer stays 16 byte aligned
    // without padding, exactly like the Linux layout (argc, argv, NULL, envp,
    // NULL, auxv).

    // envp: no environment variables.
    push(&mut cursor, 0);
    // argv: one argument, then the terminator.
    push(&mut cursor, 0);
    push(&mut cursor, name_address);
    // argc.
    push(&mut cursor, 1);

    cursor
}

/// Push one word and move the cursor down.
fn push(cursor: &mut u64, value: u64) {
    *cursor -= 8;
    // SAFETY: the stack range was mapped just before the block was written,
    // and every cursor step stays inside those pages. The volatile store keeps
    // the compiler from reordering the writes.
    unsafe {
        write_volatile(*cursor as *mut u64, value);
    }
}

/// Copy bytes to a stack address.
fn write_bytes(address: u64, bytes: &[u8]) {
    // SAFETY: `address` is inside the mapped stack pages and `bytes` does not
    // overlap them, so the copy is valid and does not alias.
    unsafe {
        copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len());
    }
}
