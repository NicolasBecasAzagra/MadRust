// SPDX-License-Identifier: GPL-2.0-only
//! Serial console access and kernel print macros.

use core::fmt::{self, Write};

use madrust_drivers::uart::{Uart16550, COM1};

/// Create a handle to the primary serial console.
pub const fn console() -> Uart16550 {
    Uart16550::new(COM1)
}

/// Format and write `args` to the serial console.
pub fn print_args(args: fmt::Arguments<'_>) {
    let mut uart = console();
    let _ = uart.write_fmt(args);
}

/// Print to the serial console without a trailing newline.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {
        $crate::console::print_args(core::format_args!($($arg)*))
    };
}

/// Print to the serial console followed by CRLF.
#[macro_export]
macro_rules! kprintln {
    () => {
        $crate::kprint!("\r\n")
    };
    ($($arg:tt)*) => {
        $crate::kprint!("{}\r\n", core::format_args!($($arg)*))
    };
}
