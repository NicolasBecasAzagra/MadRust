// SPDX-License-Identifier: GPL-2.0-only
//! Legacy virtio network device.
//!
//! The device is the transitional virtio network card (`1af4:1000`): it
//! carries the modern interface in its memory region and the legacy one in a
//! window of input and output ports. This module claims that window, switches
//! the function on, reads the legacy header and brings the device up: the
//! status register is walked, the features this milestone needs are agreed and
//! the two queues of a network card are placed in physical memory.
//!
//! Once the device is ready the driver posts buffers into the receive queue,
//! hands one frame to the transmit queue and waits for it to come back through
//! the used ring. The frame is a hand built address resolution request for the
//! gateway of the emulator, so the host has something to answer: the driver
//! then reads the answer out of the receive queue and prints what arrived.
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

use core::sync::atomic::{fence, Ordering};

use madrust_arch::pci;
use madrust_arch::port::{inb, inl, inw, outb, outl, outw};
use madrust_mm::{phys_to_virt, FRAME_SIZE};

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

/// Legacy header offset of the queue to notify.
const QUEUE_NOTIFY: u16 = 16;

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

/// Descriptor offset: the physical address of the buffer.
const DESC_ADDRESS: usize = 0;

/// Descriptor offset: the number of bytes the device may read or write.
const DESC_LENGTH: usize = 8;

/// Descriptor offset: the flags of the buffer.
const DESC_FLAGS: usize = 12;

/// Descriptor offset: the index of the next descriptor of the chain.
const DESC_NEXT: usize = 14;

/// Bytes of the header of the available ring: its flags and its index.
const AVAIL_HEADER: usize = 4;

/// Offset of the index of the available ring, the number of entries ready.
const AVAIL_INDEX: usize = 2;

/// Bytes of one entry of the available ring: the head of a chain.
const AVAIL_ENTRY: usize = 2;

/// Bytes of the header of the used ring: its flags and its index.
const USED_HEADER: usize = 4;

/// Offset of the index of the used ring, the number of entries completed.
const USED_INDEX: usize = 2;

/// Bytes of one entry of the used ring: the head of a chain and its length.
const USED_ENTRY: usize = 8;

/// Offset of the descriptor index inside one entry of the used ring.
const USED_ID: usize = 0;

/// Offset of the number of bytes written into the entry of the used ring.
const USED_LENGTH: usize = 4;

/// Descriptor flag: the device writes into the buffer instead of reading it.
const DESC_F_WRITE: u16 = 2;

/// Buffers the driver posts into the receive queue.
///
/// A frame that is never read keeps its buffer, so the count is the number of
/// frames the device may hand over before it runs out of room. The card of the
/// `q35` machine reports 256 descriptors, so the ring is far from full.
const RX_BUFFERS: usize = 32;

/// Bytes the network device reads before the frame itself: the flags, the type
/// of segmentation, and four half words with the header length, the size of
/// the segmentation, the checksum start and its offset.
///
/// The device does not segment and does not checksum, so every field is zero.
/// The header is ten bytes because the feature that adds the twelfth byte was
/// not negotiated.
const NET_HEADER: usize = 10;

/// Bytes of an Ethernet frame before its payload: the two addresses and the
/// type.
const ETHERNET_HEADER: usize = 14;

/// Smallest Ethernet frame without the check sequence: a shorter one is a runt
/// and a switch drops it.
const ETHERNET_MIN: usize = 60;

/// Type of an address resolution request.
const ARP_TYPE: u16 = 0x0806;

/// Hardware type of an address resolution request on an Ethernet link.
const ARP_ETHERNET: u16 = 1;

/// Protocol type of the address an address resolution request asks about.
const ARP_IPV4: u16 = 0x0800;

/// Operation of an address resolution request.
const ARP_REQUEST: u16 = 1;

/// Operation of an address resolution reply.
const ARP_REPLY: u16 = 2;

/// Bytes of an address resolution payload over IPv4 on an Ethernet link.
const ARP_PAYLOAD: usize = 28;

/// Offset of the operation inside an address resolution payload.
const ARP_OPERATION: usize = 6;

/// Offset of the sender hardware address inside an address resolution payload.
const ARP_SENDER_MAC: usize = 8;

/// Offset of the sender protocol address inside an address resolution payload.
const ARP_SENDER_ADDRESS: usize = 14;

/// Protocol address the emulator hands to the guest, and the one the request
/// says it comes from.
const GUEST_ADDRESS: [u8; 4] = [10, 0, 2, 15];

/// Protocol address of the gateway of the emulator, the one the host answers
/// at, and the address the request asks about.
const GATEWAY_ADDRESS: [u8; 4] = [10, 0, 2, 2];

/// Descriptor that carries the whole frame: one buffer, no chain.
const TX_DESCRIPTOR: u32 = 0;

/// How many times a poll of the used ring reads its index before it gives
/// up.
///
/// The device moves the frame as soon as it is told to, so the loop is a
/// handful of iterations in practice; the bound turns a device that never
/// answers into a reported failure instead of a boot that hangs.
const TX_POLL_LIMIT: u32 = 10_000_000;

/// How many times the receive poll reads the index of the used ring before it
/// gives up.
///
/// The host answers the address resolution request in the same turn as the
/// transmit, so the loop is short; the bound keeps a silent host from hanging
/// the boot.
const RX_POLL_LIMIT: u32 = 10_000_000;

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
    /// The device never reported the transmitted frame in the used ring.
    TransmitTimeout,
    /// The device never reported a received frame in the used ring.
    ReceiveTimeout,
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
    // negotiated. The rings stay in place for the rest of the boot.
    let rx = setup_queue(QUEUE_RX)?;
    let tx = setup_queue(QUEUE_TX)?;

    // The driver is ready only once both rings are in place, and the device
    // takes no work before it reads this bit.
    step(status, STATUS_DRIVER_OK)?;

    // The device cannot receive into a queue that holds no buffer, so the
    // empty buffers go in before the frame that asks for an answer.
    let posted = post_receive_buffers(&rx)?;
    crate::kprintln!("[virtio] rx queue={} buffers={}", QUEUE_RX, posted);

    transmit(&tx, &mac)?;
    receive(&rx)
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
fn setup_queue(index: u16) -> Result<Queue, Error> {
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
    Ok(Queue {
        base,
        descriptors: allowed,
    })
}

/// A queue whose ring sits at `base` in physical memory.
struct Queue {
    /// Physical address of the block that holds the ring.
    base: u64,
    /// Number of descriptors the device reported for the queue.
    descriptors: u16,
}

/// Offset of the available ring inside the block of a queue.
///
/// The descriptors come first, one after another, and the available ring
/// follows them.
fn avail_offset(descriptors: u16) -> usize {
    usize::from(descriptors) * DESC_SIZE
}

/// Offset of the used ring inside the block of a queue.
///
/// The standard adds one spare word after the last entry of the available ring
/// and then rounds the used ring up to the next page boundary, because the
/// device derives the whole layout from the page number.
fn used_offset(descriptors: u16) -> usize {
    (avail_offset(descriptors)
        + AVAIL_HEADER
        + usize::from(descriptors) * AVAIL_ENTRY
        + AVAIL_ENTRY)
        .next_multiple_of(FRAME_SIZE)
}

/// Hand one frame to the device and wait until it reports that it left.
fn transmit(queue: &Queue, mac: &[u8; 6]) -> Result<(), Error> {
    // The buffer is a whole frame of physical memory, because the descriptor
    // holds a physical address and the device reaches it by itself. It is
    // zeroed, so the ten bytes in front of the frame are already the header of
    // a frame with no segmentation and no checksum.
    let buffer = memory::with(|manager| manager.alloc_zeroed_frame())
        .flatten()
        .ok_or(Error::OutOfMemory)?;

    let frame = arp_request(mac);
    // SAFETY: the buffer is a page the driver owns, the frame is `NET_HEADER`
    // bytes shorter than the page, the source lives on the stack of this
    // function, and the two regions cannot overlap: one is physical memory
    // reached through the direct map and the other is the kernel stack.
    unsafe {
        core::ptr::copy_nonoverlapping(frame.as_ptr(), slot::<u8>(buffer, NET_HEADER), frame.len());
    }

    let length = (NET_HEADER + frame.len()) as u32;
    let avail = avail_offset(queue.descriptors);
    let used = used_offset(queue.descriptors);

    // One descriptor carries the whole frame. The device reads it, so the flag
    // that marks a buffer for writing stays clear, and no other descriptor
    // follows, so the flag of the chain stays clear too.
    // SAFETY: the offsets of every field lie inside descriptor zero, which
    // itself lies inside the ring block the driver owns.
    unsafe {
        core::ptr::write_volatile(slot::<u64>(queue.base, DESC_ADDRESS), buffer);
        core::ptr::write_volatile(slot::<u32>(queue.base, DESC_LENGTH), length);
        core::ptr::write_volatile(slot::<u16>(queue.base, DESC_FLAGS), 0);
        core::ptr::write_volatile(slot::<u16>(queue.base, DESC_NEXT), 0);
    }

    // The device must see the descriptor before it sees the entry that points
    // at it. A full fence leaves no room for the compiler or the processor to
    // reorder the two stores.
    fence(Ordering::SeqCst);
    // SAFETY: entry zero of the available ring is inside the ring block, and
    // it is the descriptor the driver just wrote.
    unsafe {
        core::ptr::write_volatile(
            slot::<u16>(queue.base, avail + AVAIL_HEADER),
            TX_DESCRIPTOR as u16,
        );
        core::ptr::write_volatile(slot::<u16>(queue.base, avail + AVAIL_INDEX), 1);
    }
    fence(Ordering::SeqCst);

    // The index of the available ring only says that the entry is there. The
    // write of the queue number into the notify word is what makes the device
    // read it.
    outw(BAR_BASE + QUEUE_NOTIFY, QUEUE_TX);

    // The device reports a frame by writing its descriptor index and length
    // into the used ring and then advancing the index of that ring. The index
    // is the number of frames the queue has moved, so it starts at zero and a
    // value other than zero is the frame of this call.
    let mut spins = 0;
    // SAFETY: the index of the used ring lies inside the ring block, and the
    // read is volatile so the loop sees the word the device writes.
    while unsafe { core::ptr::read_volatile(slot::<u16>(queue.base, used + USED_INDEX)) } == 0 {
        if spins == TX_POLL_LIMIT {
            free_frame(buffer);
            return Err(Error::TransmitTimeout);
        }
        spins += 1;
        core::hint::spin_loop();
    }

    // SAFETY: the device advanced the used ring, so entry zero is written and
    // its two words lie inside the ring block.
    let (descriptor, moved, count) = unsafe {
        (
            core::ptr::read_volatile(slot::<u32>(queue.base, used + USED_HEADER + USED_ID)),
            core::ptr::read_volatile(slot::<u32>(queue.base, used + USED_HEADER + USED_LENGTH)),
            core::ptr::read_volatile(slot::<u16>(queue.base, used + USED_INDEX)),
        )
    };

    // The length in the used ring is the one the device chose to report. A
    // network card has nothing to report for a transmitted frame, because the
    // buffers belong to the driver, so the card of the emulator writes zero
    // there while the count of frames is the entry of the used ring itself.
    crate::kprintln!(
        "[virtio] tx queue={} desc={} len={} used={}",
        QUEUE_TX,
        descriptor,
        moved,
        count
    );
    // The frame is back in the used ring, so the device is done with the
    // buffer and the page returns to the allocator.
    free_frame(buffer);
    Ok(())
}

/// Give one frame back to the memory manager.
fn free_frame(physical: u64) {
    let _ = memory::with(|manager| manager.free_frame(physical));
}

/// Post empty buffers into the receive queue and tell the device.
///
/// One buffer per descriptor, each a whole page of physical memory, because
/// the descriptor holds a physical address and the device reaches it by
/// itself. The buffers belong to the device until the frame they hold is read,
/// so they are not returned to the allocator here.
///
/// Returns the number of buffers that were posted.
fn post_receive_buffers(queue: &Queue) -> Result<usize, Error> {
    let count = usize::from(queue.descriptors).min(RX_BUFFERS);
    if count == 0 {
        return Err(Error::QueueSizeUnsupported);
    }

    let avail = avail_offset(queue.descriptors);
    for index in 0..count {
        let buffer = memory::with(|manager| manager.alloc_zeroed_frame())
            .flatten()
            .ok_or(Error::OutOfMemory)?;

        // The device writes into the buffer, so the descriptor carries the
        // flag that marks it for writing, and no chain follows it.
        // SAFETY: descriptor `index` and its four fields lie inside the ring
        // block the driver owns, and the buffer is a page it just took from
        // the allocator.
        unsafe {
            let descriptor = index * DESC_SIZE;
            core::ptr::write_volatile(slot::<u64>(queue.base, descriptor + DESC_ADDRESS), buffer);
            core::ptr::write_volatile(
                slot::<u32>(queue.base, descriptor + DESC_LENGTH),
                FRAME_SIZE as u32,
            );
            core::ptr::write_volatile(
                slot::<u16>(queue.base, descriptor + DESC_FLAGS),
                DESC_F_WRITE,
            );
            core::ptr::write_volatile(slot::<u16>(queue.base, descriptor + DESC_NEXT), 0);
        }

        // SAFETY: entry `index` of the available ring lies inside the ring
        // block, and it names the descriptor that was just written.
        unsafe {
            core::ptr::write_volatile(
                slot::<u16>(queue.base, avail + AVAIL_HEADER + index * AVAIL_ENTRY),
                index as u16,
            );
        }
    }

    // The descriptors must be in place before the device reads the index that
    // publishes them.
    fence(Ordering::SeqCst);
    // SAFETY: the index of the available ring lies inside the ring block.
    unsafe {
        core::ptr::write_volatile(slot::<u16>(queue.base, avail + AVAIL_INDEX), count as u16);
    }
    fence(Ordering::SeqCst);

    outw(BAR_BASE + QUEUE_NOTIFY, QUEUE_RX);
    Ok(count)
}

/// Read every frame the device reported in the used ring of the receive queue.
///
/// The device writes the descriptor it filled and the number of bytes it wrote
/// into one entry of the used ring, and then advances the index of that ring.
/// The entries between the old index and the new one are the frames that
/// arrived; [`describe`] says what each one is.
fn receive(queue: &Queue) -> Result<(), Error> {
    let used = used_offset(queue.descriptors);

    // The ring is empty when the queue is set up, so the first index the
    // device publishes is the first frame.
    let mut completed = 0u16;
    let mut spins = 0u32;
    let available = loop {
        // SAFETY: the index of the used ring lies inside the ring block, and
        // the read is volatile so the loop sees the word the device writes.
        let available =
            unsafe { core::ptr::read_volatile(slot::<u16>(queue.base, used + USED_INDEX)) };
        if available != completed {
            break available;
        }
        if spins == RX_POLL_LIMIT {
            return Err(Error::ReceiveTimeout);
        }
        spins += 1;
        core::hint::spin_loop();
    };

    while completed != available {
        describe(queue, used, completed);
        completed = completed.wrapping_add(1);
    }
    Ok(())
}

/// Say what one completed entry of the receive queue holds.
///
/// A frame the driver cannot make sense of is reported with the reason instead
/// of being dropped without a word.
fn describe(queue: &Queue, used: usize, entry: u16) {
    let count = queue.descriptors;
    // The index of the used ring counts completions, not slots, so the entry
    // wraps with the size of the ring.
    let offset = used + USED_HEADER + usize::from(entry % count) * USED_ENTRY;

    // SAFETY: the entry lies inside the used ring, which lies inside the ring
    // block the driver owns.
    let (descriptor, moved) = unsafe {
        (
            core::ptr::read_volatile(slot::<u32>(queue.base, offset + USED_ID)),
            core::ptr::read_volatile(slot::<u32>(queue.base, offset + USED_LENGTH)),
        )
    };

    if descriptor >= u32::from(count) {
        crate::kprintln!(
            "[virtio] rx queue={} desc={} len={} ignored: the descriptor is not one of the queue",
            QUEUE_RX,
            descriptor,
            moved
        );
        return;
    }

    // SAFETY: the descriptor index is inside the descriptor array, and its
    // first field is the address of the buffer it names.
    let buffer = unsafe {
        core::ptr::read_volatile(slot::<u64>(
            queue.base,
            descriptor as usize * DESC_SIZE + DESC_ADDRESS,
        ))
    };

    let length = moved as usize;
    if length > FRAME_SIZE {
        crate::kprintln!(
            "[virtio] rx queue={} desc={} len={} ignored: longer than the buffer of its descriptor",
            QUEUE_RX,
            descriptor,
            moved
        );
        return;
    }

    let mut frame = [0u8; ETHERNET_HEADER];
    if !copy_frame(buffer, length, 0, &mut frame) {
        crate::kprintln!(
            "[virtio] rx queue={} desc={} len={} ignored: shorter than the network and Ethernet headers",
            QUEUE_RX,
            descriptor,
            moved
        );
        return;
    }

    let ether_type = u16::from_be_bytes([frame[12], frame[13]]);
    crate::kprintln!(
        "[virtio] rx queue={} desc={} len={} type=0x{:04x} src={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        QUEUE_RX,
        descriptor,
        moved,
        ether_type,
        frame[6],
        frame[7],
        frame[8],
        frame[9],
        frame[10],
        frame[11]
    );

    if ether_type != ARP_TYPE {
        return;
    }

    // The reply of the gateway is the first answer this machine ever
    // received, so it is worth a line of its own.
    match gateway_reply(buffer, length) {
        Ok(Some(address)) => crate::kprintln!(
            "[virtio] arp reply from {}.{}.{}.{} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            GATEWAY_ADDRESS[0],
            GATEWAY_ADDRESS[1],
            GATEWAY_ADDRESS[2],
            GATEWAY_ADDRESS[3],
            address[0],
            address[1],
            address[2],
            address[3],
            address[4],
            address[5]
        ),
        Ok(None) => {}
        Err(reason) => crate::kprintln!(
            "[virtio] rx queue={} desc={} len={} ignored: {}",
            QUEUE_RX,
            descriptor,
            moved,
            reason
        ),
    }
}

/// Read the hardware address of the sender of an address resolution reply.
///
/// Returns `Ok(None)` when the frame is not a reply from the gateway, and the
/// reason when the payload cannot be read.
fn gateway_reply(buffer: u64, length: usize) -> Result<Option<[u8; 6]>, &'static str> {
    let mut payload = [0u8; ARP_PAYLOAD];
    if !copy_frame(buffer, length, ETHERNET_HEADER, &mut payload) {
        return Err("shorter than the address resolution payload");
    }

    let hardware = u16::from_be_bytes([payload[0], payload[1]]);
    let protocol = u16::from_be_bytes([payload[2], payload[3]]);
    let operation = u16::from_be_bytes([payload[ARP_OPERATION], payload[ARP_OPERATION + 1]]);
    if hardware != ARP_ETHERNET
        || protocol != ARP_IPV4
        || payload[4] != 6
        || payload[5] != 4
        || operation != ARP_REPLY
    {
        return Ok(None);
    }

    if payload[ARP_SENDER_ADDRESS..ARP_SENDER_ADDRESS + 4] != GATEWAY_ADDRESS {
        return Ok(None);
    }

    let mut address = [0u8; 6];
    address.copy_from_slice(&payload[ARP_SENDER_MAC..ARP_SENDER_MAC + 6]);
    Ok(Some(address))
}

/// Copy `into.len()` bytes of the frame at `offset` into the received buffer.
///
/// Returns `false` when the device reported less than the copy needs, or more
/// than the buffer of the descriptor holds, so a short frame never reads past
/// its end.
fn copy_frame(buffer: u64, length: usize, offset: usize, into: &mut [u8]) -> bool {
    let end = match offset.checked_add(into.len()) {
        Some(end) => end,
        None => return false,
    };
    if length < NET_HEADER + end || length > FRAME_SIZE {
        return false;
    }

    // SAFETY: the buffer is a page the driver posted, the check above keeps
    // the whole read inside the page, and every byte comes from physical
    // memory that the device wrote and the direct map translates.
    unsafe {
        for (index, byte) in into.iter_mut().enumerate() {
            *byte = core::ptr::read_volatile(slot::<u8>(buffer, NET_HEADER + offset + index));
        }
    }
    true
}

/// Build the address resolution request the driver sends: it asks the gateway
/// of the emulator for its hardware address.
///
/// The destination is the broadcast address because no hardware address is
/// known yet, the source is the hardware address the device reported, and the
/// payload follows the layout of an address resolution packet over IPv4 on an
/// Ethernet link. The request is padded to the smallest frame an Ethernet link
/// carries without the check sequence.
fn arp_request(mac: &[u8; 6]) -> [u8; ETHERNET_MIN] {
    let mut frame = [0u8; ETHERNET_MIN];

    frame[0..6].copy_from_slice(&[0xff; 6]);
    frame[6..12].copy_from_slice(mac);
    frame[12..14].copy_from_slice(&ARP_TYPE.to_be_bytes());

    // The payload starts where the header of the frame ends.
    let arp = ETHERNET_HEADER;
    frame[arp..arp + 2].copy_from_slice(&ARP_ETHERNET.to_be_bytes());
    frame[arp + 2..arp + 4].copy_from_slice(&ARP_IPV4.to_be_bytes());
    frame[arp + 4] = 6;
    frame[arp + 5] = 4;
    frame[arp + 6..arp + 8].copy_from_slice(&ARP_REQUEST.to_be_bytes());
    frame[arp + 8..arp + 14].copy_from_slice(mac);
    frame[arp + 14..arp + 18].copy_from_slice(&GUEST_ADDRESS);
    // The hardware address of the target stays zero: that is what the request
    // asks for.
    frame[arp + 24..arp + 28].copy_from_slice(&GATEWAY_ADDRESS);

    frame
}

/// Pointer to `offset` bytes into the block of physical memory at `base`.
///
/// # Safety
///
/// `base` must be the start of a block the caller owns for as long as the
/// pointer is used, and `offset` plus the size of `T` must stay inside it.
unsafe fn slot<T>(base: u64, offset: usize) -> *mut T {
    // SAFETY: the caller guarantees that the offset stays inside the block, so
    // the sum stays inside the direct map and the pointer is valid.
    unsafe { (phys_to_virt(base) as *mut u8).add(offset).cast::<T>() }
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
    used_offset(descriptors) + USED_HEADER + usize::from(descriptors) * USED_ENTRY
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
