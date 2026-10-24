// SPDX-License-Identifier: GPL-2.0-only
//! 8259 programmable interrupt controller initialisation.
//!
//! The BIOS maps the PIC to vectors 0x08-0x0F, which collide with CPU
//! exceptions. Both controllers are remapped to 0x20-0x2F and everything but
//! the timer line is masked.

use madrust_arch::port;

/// First vector of the remapped PIC range (master IRQ0).
pub const IRQ_BASE: u8 = 0x20;
/// Vector of the PIT, which is wired to IRQ0 on the PC.
pub const TIMER_VECTOR: u8 = IRQ_BASE;

const MASTER_COMMAND: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_COMMAND: u16 = 0xA0;
const SLAVE_DATA: u16 = 0xA1;

/// ICW1: start initialisation, expect ICW4.
const ICW1_INIT_ICW4: u8 = 0x11;
/// ICW3 master: slave is cascaded through IRQ2.
const ICW3_MASTER_CASCADE_IRQ2: u8 = 0x04;
/// ICW3 slave: cascade identity.
const ICW3_SLAVE_CASCADE: u8 = 0x02;
/// ICW4: 8086/88 mode.
const ICW4_8086: u8 = 0x01;
/// OCW1: mask every line.
const MASK_ALL: u8 = 0xFF;
/// OCW1: enable only IRQ0.
const MASK_TIMER_ONLY: u8 = !(1 << 0);
/// End of interrupt command.
const EOI: u8 = 0x20;

/// Short delay between ICW writes on real hardware.
fn io_wait() {
    port::outb(0x80, 0);
}

/// Remap both PICs and unmask the timer.
pub fn init() {
    port::outb(MASTER_COMMAND, ICW1_INIT_ICW4);
    io_wait();
    port::outb(SLAVE_COMMAND, ICW1_INIT_ICW4);
    io_wait();

    port::outb(MASTER_DATA, IRQ_BASE);
    io_wait();
    port::outb(SLAVE_DATA, IRQ_BASE + 8);
    io_wait();

    port::outb(MASTER_DATA, ICW3_MASTER_CASCADE_IRQ2);
    io_wait();
    port::outb(SLAVE_DATA, ICW3_SLAVE_CASCADE);
    io_wait();

    port::outb(MASTER_DATA, ICW4_8086);
    io_wait();
    port::outb(SLAVE_DATA, ICW4_8086);
    io_wait();

    port::outb(MASTER_DATA, MASK_ALL);
    port::outb(SLAVE_DATA, MASK_ALL);
    port::outb(MASTER_DATA, MASK_TIMER_ONLY);
}

/// Acknowledge an interrupt.
///
/// Any interrupt from the slave range is acknowledged on both controllers.
pub fn send_eoi(vector: u8) {
    if vector >= IRQ_BASE + 8 {
        port::outb(SLAVE_COMMAND, EOI);
    }
    port::outb(MASTER_COMMAND, EOI);
}
