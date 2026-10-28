// SPDX-License-Identifier: GPL-2.0-only
//! Enumeration of the PCI configuration space.
//!
//! The guest has no firmware, so nothing hands the kernel a list of the
//! devices of the machine: it walks bus zero itself, reads the vendor id of
//! every function through the pair of ports of the host bridge, and reports
//! what answers. The configuration space needs no interrupt, so the bus is
//! enumerated before the interrupt controller is programmed.

use madrust_arch::pci::{self, CONFIG_ADDRESS, CONFIG_DATA};
use madrust_arch::port::{inl, outl};

/// Bus the enumeration walks. A `q35` machine puts every device on it.
const BUS: u8 = 0;

/// Slots of one bus.
const SLOTS: u8 = 32;

/// Functions of one slot.
const FUNCTIONS: u8 = 8;

/// Configuration register that holds the vendor and device id.
const VENDOR_REGISTER: u8 = 0x00;

/// Configuration register that holds the revision and the class code.
const CLASS_REGISTER: u8 = 0x08;

/// Why a register of the configuration space could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No function answers at this address: the register reads all ones.
    NoDevice,
}

/// Read one 32-bit register of the configuration space of a function.
///
/// A read only fails when the function is absent, which is the normal answer
/// for most of the space; the caller decides whether that is a problem.
pub fn read_register(bus: u8, device: u8, function: u8, offset: u8) -> Result<u32, Error> {
    outl(
        CONFIG_ADDRESS,
        pci::config_address(bus, device, function, offset),
    );
    let value = inl(CONFIG_DATA);
    if value == u32::MAX {
        Err(Error::NoDevice)
    } else {
        Ok(value)
    }
}

/// Print one line per function that answers on bus zero.
///
/// The line is fixed so the boot gate can match it:
/// `[pci] <bus>:<slot>.<function> <vendor>:<device> class <class>`, with the
/// vendor, the device and the class code as hexadecimal digits.
pub fn enumerate() {
    for device in 0..SLOTS {
        for function in 0..FUNCTIONS {
            let vendor_register = match read_register(BUS, device, function, VENDOR_REGISTER) {
                Ok(value) => value,
                // The slot is empty. Function zero of an empty slot means the
                // whole device is absent, so the remaining functions are not
                // worth a port access each.
                Err(_) if function == 0 => break,
                Err(_) => continue,
            };
            let vendor = pci::vendor_id(vendor_register);
            if vendor == pci::NO_VENDOR {
                if function == 0 {
                    break;
                }
                continue;
            }
            // A function that answered the vendor read has a class code; if
            // the second read fails anyway, report the class the kernel cannot
            // know instead of dropping the device from the log.
            let class = match read_register(BUS, device, function, CLASS_REGISTER) {
                Ok(value) => pci::class_code(value),
                Err(_) => u32::MAX,
            };
            crate::kprintln!(
                "[pci] {:02x}:{:02x}.{} {:04x}:{:04x} class {:06x}",
                BUS,
                device,
                function,
                vendor,
                pci::device_id(vendor_register),
                class
            );
        }
    }
}
