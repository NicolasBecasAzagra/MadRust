// SPDX-License-Identifier: GPL-2.0-only
//! Legacy virtio network device.
//!
//! The device is the transitional virtio network card (`1af4:1000`): it
//! carries the modern interface in its memory region and the legacy one in a
//! window of input and output ports. This module claims that window, switches
//! the function on, reads the legacy header and brings the device up: the
//! status register is walked, the features this milestone needs are agreed and
//! the two queues of a network card are placed in physical memory. No frame
//! moves yet.
//!
//! The guest has no firmware, so the base address register of the card is
//! zero and the kernel assigns the port range itself. The interrupt line of
//! the function stays untouched: it is not routed yet, so the device is
//! polled.
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
//!
//! The number of descriptors is the device's to report and the driver lays the
//! ring out for it as it stands: this interface carries no queue size the
//! driver could ask for. The card of the `q35` machine reports 256 descriptors
//! for the receiving and the transmitting queue, so one ring is a block of
//! four pages. The control queue only exists when the feature that asks for it
//! is agreed, and it is not.

use madrust_arch::pci;
use madrust_arch::port::{inb, inl, inw, outb, outl, outw};
use madrust_mm::FRAME_SIZE;

use crate::memory;
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

/// Legacy header offset of the features the driver accepts.
const GUEST_FEATURES: u16 = 4;

/// Legacy header offset of the page number of the selected queue.
const QUEUE_ADDRESS: u16 = 8;

/// Legacy header offset of the number of descriptors of the selected queue.
const QUEUE_SIZE: u16 = 12;

/// Legacy header offset of the queue to select.
const QUEUE_SELECT: u16 = 14;

/// Legacy header offset of the device status.
const DEVICE_STATUS: u16 = 18;

/// Legacy header offset of the hardware address of the device.
const DEVICE_CONFIG: u16 = 20;

/// Status bit: the driver has seen the device.
const STATUS_ACKNOWLEDGE: u8 = 1 << 0;

/// Status bit: the driver knows how to drive the device.
const STATUS_DRIVER: u8 = 1 << 1;

/// Status bit: the driver is ready and the device may run.
const STATUS_DRIVER_OK: u8 = 1 << 2;

/// Status bit: the driver and the device agreed on the features.
const STATUS_FEATURES_OK: u8 = 1 << 3;

/// Feature bit 5: the device carries its own hardware address.
const FEATURE_MAC: u32 = 1 << 5;

/// Feature bit 16: the device reports the state of the link.
const FEATURE_STATUS: u32 = 1 << 16;

/// Features the driver and the device agree on.
///
/// The two words of this interface are thirty two bits wide, so nothing above
/// bit 31 can be negotiated through it. Neither the hardware address nor the
/// link status needs more.
const NEGOTIATED: u32 = FEATURE_MAC | FEATURE_STATUS;

/// Number of the queue that receives frames.
const QUEUE_RX: u16 = 0;

/// Number of the queue that transmits frames.
const QUEUE_TX: u16 = 1;

/// Bytes of one descriptor: the address of the buffer, its length, the flags
/// and the index of the next descriptor.
const DESC_SIZE: usize = 16;

/// Bytes of the header of the available ring: its flags and its index.
const AVAIL_HEADER: usize = 4;

/// Bytes of one entry of the available ring: the head of a chain.
const AVAIL_ENTRY: usize = 2;

/// Bytes of the header of the used ring: its flags and its index.
const USED_HEADER: usize = 4;

/// Bytes of one entry of the used ring: the head of a chain and its length.
const USED_ENTRY: usize = 8;

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
    /// The status register did not read zero after the reset.
    NotReset,
    /// The device did not keep a status bit the driver set.
    StatusRejected,
    /// The device does not offer a feature this milestone needs.
    FeatureMissing,
    /// The device did not keep the features the driver accepted.
    FeaturesRejected,
    /// The selected queue reports no size, or one that is not a power of two.
    QueueSizeUnsupported,
    /// No contiguous physical block is free for the ring of the queue.
    OutOfMemory,
    /// The page number of the ring does not fit the thirty two bit field.
    QueueAddressOutOfRange,
    /// The device did not keep the page number of the queue.
    QueueRejected,
    /// The number of descriptors changed once the page number was in place.
    QueueSizeChanged,
}

impl From<ConfigError> for Error {
    fn from(_: ConfigError) -> Self {
        Error::Config
    }
}

/// Find the network card, claim its legacy window, bring it up and set up its
/// queues.
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

    // From here the device is reset and the status register grows one bit at a
    // time. The device clears a bit it refuses, so every step is read back.
    reset()?;
    let status = step(0, STATUS_ACKNOWLEDGE)?;
    let status = step(status, STATUS_DRIVER)?;

    negotiate(features)?;
    let status = step(status, STATUS_FEATURES_OK)?;

    // A network device has two queues, one per direction. The control queue
    // would only exist with the feature that asks for it, and it is not
    // negotiated. The rings stay in place for the rest of the boot: the next
    // step of the milestone fills them with descriptors.
    for index in [QUEUE_RX, QUEUE_TX] {
        setup_queue(index)?;
    }

    // The driver is ready only once both rings are in place.
    step(status, STATUS_DRIVER_OK)?;
    Ok(())
}

/// Reset the device and check that the status register reads zero.
fn reset() -> Result<(), Error> {
    outb(BAR_BASE + DEVICE_STATUS, 0);
    if inb(BAR_BASE + DEVICE_STATUS) != 0 {
        return Err(Error::NotReset);
    }
    Ok(())
}

/// Set one more status bit and read the whole register back.
///
/// A bit the device does not keep is a step the device did not accept.
fn step(current: u8, bit: u8) -> Result<u8, Error> {
    let status = current | bit;
    outb(BAR_BASE + DEVICE_STATUS, status);
    if inb(BAR_BASE + DEVICE_STATUS) & status != status {
        return Err(Error::StatusRejected);
    }
    Ok(status)
}

/// Accept the features this milestone needs and read them back.
fn negotiate(offered: u32) -> Result<(), Error> {
    if offered & NEGOTIATED != NEGOTIATED {
        return Err(Error::FeatureMissing);
    }
    outl(BAR_BASE + GUEST_FEATURES, NEGOTIATED);
    if inl(BAR_BASE + GUEST_FEATURES) & NEGOTIATED != NEGOTIATED {
        return Err(Error::FeaturesRejected);
    }
    Ok(())
}

/// Select one queue, place its ring in physical memory and tell the device.
///
/// The device derives the whole layout of the ring from the page number, so
/// the queue is selected before anything else and the page number arrives
/// last: an address that comes in while another queue is selected is taken
/// for that queue.
fn setup_queue(index: u16) -> Result<(), Error> {
    outw(BAR_BASE + QUEUE_SELECT, index);

    let allowed = inw(BAR_BASE + QUEUE_SIZE);
    if allowed == 0 || !allowed.is_power_of_two() {
        return Err(Error::QueueSizeUnsupported);
    }

    // The ring lives in one contiguous block of physical memory: the device
    // reaches it by itself, and the descriptors hold physical addresses.
    let order = block_order(ring_bytes(allowed));
    let base = memory::with(|manager| manager.alloc_zeroed_block(order))
        .flatten()
        .ok_or(Error::OutOfMemory)?;

    let page =
        u32::try_from(base / FRAME_SIZE as u64).map_err(|_| Error::QueueAddressOutOfRange)?;
    outl(BAR_BASE + QUEUE_ADDRESS, page);
    if inl(BAR_BASE + QUEUE_ADDRESS) != page {
        free(base, order);
        return Err(Error::QueueRejected);
    }

    // The size decides where every part of the ring sits, so it must not
    // change once the page number is in place.
    let max = inw(BAR_BASE + QUEUE_SIZE);
    if max != allowed {
        free(base, order);
        return Err(Error::QueueSizeChanged);
    }

    crate::kprintln!(
        "[virtio] queue {} base=0x{:012x} descriptors={} max={}",
        index,
        base,
        allowed,
        max
    );
    Ok(())
}

/// Give a ring block back to the memory manager.
fn free(base: u64, order: u8) {
    let _ = memory::with(|manager| manager.free_block(base, order));
}

/// Number of bytes a ring of `descriptors` descriptors needs.
///
/// The layout is the one of `include/standard-headers/linux/virtio_ring.h`:
/// the descriptor array, then the available ring, and then the used ring at
/// the next page boundary, because the device derives the whole layout from
/// the page number of the block and aligns the used ring to the page size.
fn ring_bytes(descriptors: u16) -> usize {
    let count = usize::from(descriptors);
    let avail_end = count * DESC_SIZE + AVAIL_HEADER + count * AVAIL_ENTRY;
    // The standard adds one spare word after the last entry of the available
    // ring before it rounds the used ring up to the page.
    let used = (avail_end + AVAIL_ENTRY).next_multiple_of(FRAME_SIZE);
    used + USED_HEADER + count * USED_ENTRY
}

/// Smallest buddy order whose block holds `bytes` bytes.
fn block_order(bytes: usize) -> u8 {
    let pages = bytes.div_ceil(FRAME_SIZE);
    pages.next_power_of_two().trailing_zeros() as u8
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
