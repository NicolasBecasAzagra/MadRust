// SPDX-License-Identifier: GPL-2.0-only
//! Port-mapped I/O accessors.

#[cfg(target_arch = "x86_64")]
mod imp {
    use core::arch::asm;

    /// Read one byte from `port`.
    pub fn inb(port: u16) -> u8 {
        let value: u8;
        // SAFETY: `in al, dx` only reads from the I/O port space, cannot
        // corrupt memory, and does not require any operands to be valid
        // beyond the register constraints encoded here.
        unsafe {
            asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// Read one 16-bit word from `port`.
    pub fn inw(port: u16) -> u16 {
        let value: u16;
        // SAFETY: see [`inb`]; the instruction touches I/O ports only.
        unsafe {
            asm!("in ax, dx", out("ax") value, in("dx") port, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// Read one 32-bit double word from `port`.
    pub fn inl(port: u16) -> u32 {
        let value: u32;
        // SAFETY: see [`inb`]; the instruction touches I/O ports only.
        unsafe {
            asm!("in eax, dx", out("eax") value, in("dx") port, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// Write one byte to `port`.
    pub fn outb(port: u16, value: u8) {
        // SAFETY: `out dx, al` only writes to the I/O port space; it cannot
        // corrupt memory and has no register side effects.
        unsafe {
            asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
        }
    }

    /// Write one 16-bit word to `port`.
    pub fn outw(port: u16, value: u16) {
        // SAFETY: see [`outb`]; the instruction touches I/O ports only.
        unsafe {
            asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags));
        }
    }

    /// Write one 32-bit double word to `port`.
    pub fn outl(port: u16, value: u32) {
        // SAFETY: see [`outb`]; the instruction touches I/O ports only.
        unsafe {
            asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod imp {
    /// Host stub: reports zero bytes read.
    pub fn inb(_port: u16) -> u8 {
        0
    }

    /// Host stub: reports zero words read.
    pub fn inw(_port: u16) -> u16 {
        0
    }

    /// Host stub: reports zero double words read.
    pub fn inl(_port: u16) -> u32 {
        0
    }

    /// Host stub: performs no I/O.
    pub fn outb(_port: u16, _value: u8) {}

    /// Host stub: performs no I/O.
    pub fn outw(_port: u16, _value: u16) {}

    /// Host stub: performs no I/O.
    pub fn outl(_port: u16, _value: u32) {}
}

pub use imp::{inb, inl, inw, outb, outl, outw};
