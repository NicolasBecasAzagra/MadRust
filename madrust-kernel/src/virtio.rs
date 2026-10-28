// SPDX-License-Identifier: GPL-2.0-only
//! Legacy virtio network device.
//!
//! The device is the transitional virtio network card (`1af4:1000`): it
//! carries the modern interface in its memory region and the legacy one in a
//! window of input and output ports. This module claims that window, switches
//! the function on and reads the legacy header, which the next step of the
//! milestone needs to set up a queue.
//!
//! The guest has no firmware, so the base address register of the card is
//! zero and the kernel assigns the port range itself. The interrupt line of
//! the function stays untouched: it is not routed yet, so the header is read
//! by polling the window.
//!
//! Layout of the legacy header, at the beginning of the port window:
//!
//! | offset | size | meaning                                   |
//! |--------|------|-------------------------------------------|
//! | 0      | 4    | features offered by the device, read only |
//! | 4      | 4    | features accepted by the driver           |
//! | 8      | 4    | page number of the queue                  |
//! | 12     | 2    | number of descriptors in the queue        |
//! | 14     | 2    | queue to select                           |
//! | 16     | 2    | queue to notify                           |
//! | 18     | 1    | device status                             |
//! | 19     | 1    | interrupt status, cleared by the read     |
//! | 20     | 6    | hardware address of the device            |
//! | 26     | 2    | link status                               |
//! | 28     | 2    | number of queue pairs                     |

use madrust_arch::pci;
use madrust_arch::port::{inb, inl};

use crate::pci::{read_register, write_register, Error as ConfigError};

/// Vendor id of a virtio device.
const VENDOR: u16 = 0x1af4;

/// Device id of the transitional virtio network card.
const DEVICE: u16 = 0x1000;

/// Bus the enumeration walks, and the only one a `q35` machine has.
const BUS: u8 = 0;

/// Slots of one bus.
const SLOTS: u8 = 32;

/// Functions of one slot.
const FUNCTIONS: u8 = 8;

/// Configuration register with the vendor and device id.
const VENDOR_REGISTER: u8 = 0x00;

/// Configuration register with the command and status words.
const COMMAND_REGISTER: u8 = 0x04;

/// Configuration register of the first base address, which is the legacy
/// window of input and output ports.
const BAR0_REGISTER: u8 = 0x10;

/// First bit of a base address register: the region is input and output
/// ports, not memory.
const BAR_IO: u32 = 0x1;

/// Base the kernel assigns to the window of the device.
///
/// The range `0xc000` to `0xc0ff` is the block the firmware of the `q35`
/// machine gives to the input and output regions of the devices on the bus.
/// No firmware runs in this guest, so nothing has claimed it: it sits above
/// the legacy ports of the kernel (the interrupt controllers at `0x20`, the
/// timer at `0x40`, the serial port at `0x3f8` and the debug exit at `0xf4`)
/// and below the configuration ports at `0xcf8`.
const BAR_BASE: u16 = 0xc000;

/// Size of the port range reserved at [`BAR_BASE`].
///
/// The device of the `q35` machine asks for 32 bytes, and the block is large
/// enough for a device that asks for more, as long as it stays below the
/// configuration ports at `0xcf8`.
const BAR_LIMIT: u32 = 0x100;

/// Width of the mask that hides the flags of a base address register.
const BAR_FLAGS: u32 = 0xf;

/// Command register bit: the function decodes its input and output region.
const COMMAND_IO: u16 = 1 << 0;

/// Command register bit: the function decodes its memory region.
const COMMAND_MEMORY: u16 = 1 << 1;

/// Command register bit: the function may read and write guest memory.
const COMMAND_BUS_MASTER: u16 = 1 << 2;

/// Legacy header offset of the features the device offers.
const DEVICE_FEATURES: u16 = 0;

/// Legacy header offset of the hardware address of the device.
const DEVICE_CONFIG: u16 = 20;

/// Why a probe failed. A card that is absent is not an error: the caller
/// boots without a network device on every machine that has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The function stopped answering while its configuration was read.
    Config,
    /// The first base address register does not map input and output ports.
    BarNotIo,
    /// The device asked for a region the kernel cannot place at [`BAR_BASE`].
    BarUnsupported,
    /// The command register did not keep the bits the function needs.
    CommandRejected,
    /// The window reads all ones: the function does not decode its ports.
    NotResponding,
}

impl From<ConfigError> for Error {
    fn from(_: ConfigError) -> Self {
        Error::Config
    }
}

/// Find the network card, claim its legacy window and report what it is.
///
/// Returns `Ok(())` when the bus carries no such card, so a machine without
/// one boots as before.
pub fn probe() -> Result<(), Error> {
    let Some((device, function)) = find() else {
        return Ok(());
    };

    let size = size_bar(device, function)?;
    if size > BAR_LIMIT {
        return Err(Error::BarUnsupported);
    }

    // Hand the window to the function. The low bit keeps the region marked as
    // input and output ports; the base itself is four byte aligned.
    write_register(
        BUS,
        device,
        function,
        BAR0_REGISTER,
        u32::from(BAR_BASE) | BAR_IO,
    );

    enable(device, function)?;

    // The card answers through its window only after the command register
    // turns the decoding on. An open bus reads all ones.
    let features = inl(BAR_BASE + DEVICE_FEATURES);
    if features == u32::MAX {
        return Err(Error::NotResponding);
    }

    let mut mac = [0u8; 6];
    for (index, byte) in mac.iter_mut().enumerate() {
        // The hardware address is six bytes at the start of the configuration
        // of the device, so it is read one port at a time.
        *byte = inb(BAR_BASE + DEVICE_CONFIG + index as u16);
    }

    crate::kprintln!(
        "[virtio] net base=0x{:04x} size={} features=0x{:08x} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        BAR_BASE,
        size,
        features,
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );
    Ok(())
}

/// Walk bus zero and return the slot and function of the network card.
fn find() -> Option<(u8, u8)> {
    for device in 0..SLOTS {
        for function in 0..FUNCTIONS {
            let register = match read_register(BUS, device, function, VENDOR_REGISTER) {
                Ok(value) => value,
                // Function zero of an empty slot means the whole device is
                // absent, so the remaining functions are skipped.
                Err(_) if function == 0 => break,
                Err(_) => continue,
            };
            if pci::vendor_id(register) == VENDOR && pci::device_id(register) == DEVICE {
                return Some((device, function));
            }
            // A vendor of all ones is the marker of an empty slot.
            if pci::vendor_id(register) == pci::NO_VENDOR && function == 0 {
                break;
            }
        }
    }
    None
}

/// Ask the device how large its first region is.
///
/// Writing all ones to the register makes the function report the size in the
/// bits it decodes: the low bit keeps the kind of region and the rest form a
/// mask over the address bits, so the size is the complement of the mask plus
/// one.
fn size_bar(device: u8, function: u8) -> Result<u32, Error> {
    write_register(BUS, device, function, BAR0_REGISTER, u32::MAX);
    let sized = read_register(BUS, device, function, BAR0_REGISTER)?;
    if sized & BAR_IO == 0 {
        return Err(Error::BarNotIo);
    }

    // Drop the flags and the reserved low bits, then invert the mask: the
    // device asked for that many bytes.
    let mask = sized & !BAR_FLAGS;
    let size = (!mask).wrapping_add(1);
    if size == 0 || !size.is_power_of_two() {
        return Err(Error::BarUnsupported);
    }
    Ok(size)
}

/// Set the command bits the function needs and read them back.
///
/// The card decodes its port window, decodes its memory region where the
/// modern interface lives, and must be a bus master to reach the queues in
/// guest memory later. The upper half of the register is the status word and
/// stays as the device reports it.
fn enable(device: u8, function: u8) -> Result<(), Error> {
    let register = read_register(BUS, device, function, COMMAND_REGISTER)?;
    let command = (register as u16) | COMMAND_IO | COMMAND_MEMORY | COMMAND_BUS_MASTER;
    write_register(
        BUS,
        device,
        function,
        COMMAND_REGISTER,
        (register & 0xffff_0000) | u32::from(command),
    );

    let readback = read_register(BUS, device, function, COMMAND_REGISTER)? as u16;
    if readback & command != command {
        return Err(Error::CommandRejected);
    }
    Ok(())
}
