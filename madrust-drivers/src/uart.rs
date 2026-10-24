// SPDX-License-Identifier: GPL-2.0-only
//! 16550A-compatible UART driver.

use core::fmt;

use madrust_arch::port;

/// I/O base address of the first serial port (COM1).
pub const COM1: u16 = 0x3F8;

/// Divisor latch value for 115200 baud (base clock / (16 * baud)).
const DIVISOR_115200: u16 = 1;

/// Line status register: transmit holding register is empty.
const LSR_THRE: u8 = 1 << 5;
/// Line status register: transmitter is completely empty.
const LSR_TEMT: u8 = 1 << 6;

/// Register offsets from the UART base address.
const REG_DATA: u16 = 0;
const REG_IER: u16 = 1;
const REG_FCR: u16 = 2;
const REG_LCR: u16 = 3;
const REG_MCR: u16 = 4;
const REG_LSR: u16 = 5;

/// Line control register: divisor latch access bit.
const LCR_DLAB: u8 = 1 << 7;
/// Line control register: 8 bits, no parity, one stop bit.
const LCR_8N1: u8 = 0x03;
/// FIFO control register: enable and clear FIFOs, 14-byte trigger level.
const FCR_ENABLE_CLEAR_14: u8 = 0xC7;
/// Modem control register: data terminal ready and request to send.
const MCR_DTR_RTS: u8 = 0x03;

/// A 16550A-compatible UART.
#[derive(Clone, Copy, Debug)]
pub struct Uart16550 {
    base: u16,
}

impl Uart16550 {
    /// Create a driver for the UART mapped at `base`.
    pub const fn new(base: u16) -> Self {
        Self { base }
    }

    /// Base I/O port of this UART.
    pub const fn base(&self) -> u16 {
        self.base
    }

    /// Configure the UART: 115200 baud, 8N1, FIFOs on, interrupts disabled.
    pub fn init(&self) {
        port::outb(self.base + REG_IER, 0x00);
        port::outb(self.base + REG_LCR, LCR_DLAB);
        port::outb(self.base + REG_DATA, DIVISOR_115200 as u8);
        port::outb(self.base + REG_IER, (DIVISOR_115200 >> 8) as u8);
        port::outb(self.base + REG_LCR, LCR_8N1);
        port::outb(self.base + REG_FCR, FCR_ENABLE_CLEAR_14);
        port::outb(self.base + REG_MCR, MCR_DTR_RTS);
    }

    /// Wait until the transmitter can accept a byte.
    fn wait_tx_ready(&self) {
        while port::inb(self.base + REG_LSR) & LSR_THRE == 0 {
            core::hint::spin_loop();
        }
    }

    /// Wait until the transmitter has fully drained.
    pub fn flush(&self) {
        while port::inb(self.base + REG_LSR) & LSR_TEMT == 0 {
            core::hint::spin_loop();
        }
    }

    /// Send one byte, blocking while the FIFO is full.
    pub fn write_byte(&mut self, byte: u8) {
        self.wait_tx_ready();
        port::outb(self.base + REG_DATA, byte);
    }

    /// Send a string as-is; no newline translation is performed.
    pub fn write_str(&mut self, text: &str) {
        for byte in text.bytes() {
            self.write_byte(byte);
        }
    }

    /// Send a raw byte slice.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_byte(*byte);
        }
    }
}

impl fmt::Write for Uart16550 {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        Self::write_str(self, text);
        Ok(())
    }
}
