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
//! Once the device is ready the driver posts buffers into the receive queue and
//! returns the device to the caller. The frames belong to [`crate::net`]: it
//! builds them with the protocols of `madrust_net`, hands one to
//! [`Device::transmit`] and reads the answers with [`Device::poll`].
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

/// Descriptor that carries the whole frame: one buffer, no chain.
const TX_DESCRIPTOR: u32 = 0;

/// How many times a poll of the used ring reads its index before it gives
/// up.
///
/// The device moves the frame as soon as it is told to, so the loop is a
/// handful of iterations in practice; the bound turns a device that never
/// answers into a reported failure instead of a boot that hangs.
const TX_POLL_LIMIT: u32 = 10_000_000;

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
    /// The caller could not write a whole frame into the buffer of the device.
    FrameRejected,
}

impl From<ConfigError> for Error {
    fn from(_: ConfigError) -> Self {
        Error::Config
    }
}

/// Find the network card, claim its legacy window, bring it up and set up its
/// queues.
///
/// Returns `Ok(None)` when the bus carries no such card, so a machine without
/// one boots as before.
pub fn probe() -> Result<Option<Device>, Error> {
    let Some((device, function)) = find() else {
        return Ok(None);
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
    // empty buffers go in before the caller sends anything.
    let posted = post_receive_buffers(&rx)?;
    crate::kprintln!("[virtio] rx queue={} buffers={}", QUEUE_RX, posted);

    Ok(Some(Device {
        mac,
        rx,
        tx,
        rx_used: 0,
        rx_avail: posted as u16,
        tx_used: 0,
    }))
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

/// The network card once it is up: its hardware address and the two queues.
///
/// Nothing routes the interrupt of the function, so the card is polled:
/// [`Device::poll`] reads the frames that arrived and [`Device::transmit`]
/// hands one frame over and waits until the device reports that it left.
pub struct Device {
    /// Hardware address the device reported.
    mac: [u8; 6],
    /// Queue that receives frames.
    rx: Queue,
    /// Queue that transmits frames.
    tx: Queue,
    /// Completions of the receive queue already read.
    rx_used: u16,
    /// Next value of the index of the available ring of the receive queue.
    rx_avail: u16,
    /// Completions of the transmit queue already read.
    tx_used: u16,
}

impl Device {
    /// Hardware address of the device.
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// Hand one frame to the device and wait until it reports that it left.
    ///
    /// `build` writes the frame into the buffer of the device and returns its
    /// length, or `None` when the frame does not fit and nothing is sent. The
    /// descriptor of the buffer holds a physical address, so the device reaches
    /// the bytes by itself and the closure writes the frame where the device
    /// will read it, without a second copy.
    pub fn transmit(
        &mut self,
        build: impl FnOnce(&mut [u8]) -> Option<usize>,
    ) -> Result<(), Error> {
        // The buffer is a whole frame of physical memory. It is zeroed, so the
        // ten bytes in front of the frame are already the header of a frame
        // with no segmentation and no checksum.
        let buffer = memory::with(|manager| manager.alloc_zeroed_frame())
            .flatten()
            .ok_or(Error::OutOfMemory)?;

        // SAFETY: the buffer is a page the driver owns for the rest of this
        // call, and the frame area starts after the header of the device and
        // ends at the end of that page, so the whole slice is inside it.
        let area = unsafe {
            core::slice::from_raw_parts_mut(slot::<u8>(buffer, NET_HEADER), FRAME_SIZE - NET_HEADER)
        };
        let Some(frame) = build(area) else {
            free_frame(buffer);
            return Err(Error::FrameRejected);
        };
        if frame > FRAME_SIZE - NET_HEADER {
            free_frame(buffer);
            return Err(Error::FrameRejected);
        }

        let length = (NET_HEADER + frame) as u32;
        let avail = avail_offset(self.tx.descriptors);
        let used = used_offset(self.tx.descriptors);

        // One descriptor carries the whole frame. The device reads it, so the
        // flag that marks a buffer for writing stays clear, and no other
        // descriptor follows, so the flag of the chain stays clear too.
        // SAFETY: the offsets of every field lie inside descriptor zero, which
        // itself lies inside the ring block the driver owns.
        unsafe {
            core::ptr::write_volatile(slot::<u64>(self.tx.base, DESC_ADDRESS), buffer);
            core::ptr::write_volatile(slot::<u32>(self.tx.base, DESC_LENGTH), length);
            core::ptr::write_volatile(slot::<u16>(self.tx.base, DESC_FLAGS), 0);
            core::ptr::write_volatile(slot::<u16>(self.tx.base, DESC_NEXT), 0);
        }

        // The device must see the descriptor before it sees the entry that
        // points at it. A full fence leaves no room for the compiler or the
        // processor to reorder the two stores.
        fence(Ordering::SeqCst);
        // The index of the available ring counts the frames the driver has
        // handed over, so the entry of this one sits where that count points.
        let entry = usize::from(self.tx_used % self.tx.descriptors);
        // SAFETY: entry `entry` of the available ring lies inside the ring
        // block, and it names the descriptor the driver just wrote.
        unsafe {
            core::ptr::write_volatile(
                slot::<u16>(self.tx.base, avail + AVAIL_HEADER + entry * AVAIL_ENTRY),
                TX_DESCRIPTOR as u16,
            );
        }
        fence(Ordering::SeqCst);
        let index = self.tx_used.wrapping_add(1);
        // SAFETY: the index of the available ring lies inside the ring block.
        unsafe {
            core::ptr::write_volatile(slot::<u16>(self.tx.base, avail + AVAIL_INDEX), index);
        }
        fence(Ordering::SeqCst);

        // The index of the available ring only says that the entry is there.
        // The write of the queue number into the notify word is what makes the
        // device read it.
        outw(BAR_BASE + QUEUE_NOTIFY, QUEUE_TX);

        // The device reports a frame by writing its descriptor index and length
        // into the used ring and then advancing the index of that ring. The
        // index is the number of frames the queue has moved, so the frame of
        // this call is the one that brings it to `index`.
        let mut spins = 0;
        // SAFETY: the index of the used ring lies inside the ring block, and
        // the read is volatile so the loop sees the word the device writes.
        while unsafe { core::ptr::read_volatile(slot::<u16>(self.tx.base, used + USED_INDEX)) }
            != index
        {
            if spins == TX_POLL_LIMIT {
                free_frame(buffer);
                return Err(Error::TransmitTimeout);
            }
            spins += 1;
            core::hint::spin_loop();
        }

        let offset =
            used + USED_HEADER + usize::from(self.tx_used % self.tx.descriptors) * USED_ENTRY;
        // SAFETY: the device advanced the used ring, so the entry of this frame
        // is written and its two words lie inside the ring block.
        let (descriptor, moved) = unsafe {
            (
                core::ptr::read_volatile(slot::<u32>(self.tx.base, offset + USED_ID)),
                core::ptr::read_volatile(slot::<u32>(self.tx.base, offset + USED_LENGTH)),
            )
        };
        self.tx_used = index;

        // The length in the used ring is the one the device chose to report. A
        // network card has nothing to report for a transmitted frame, because
        // the buffers belong to the driver, so the card of the emulator writes
        // zero there while the count of frames is the entry of the used ring
        // itself.
        crate::kprintln!(
            "[virtio] tx queue={} desc={} len={} used={}",
            QUEUE_TX,
            descriptor,
            moved,
            index
        );
        // The frame is back in the used ring, so the device is done with the
        // buffer and the page returns to the allocator.
        free_frame(buffer);
        Ok(())
    }
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

impl Device {
    /// Read every frame the device reported since the last call.
    ///
    /// `handler` is called once per frame with the bytes as the device wrote them:
    /// the length is the one the device reported, so a frame shorter than the
    /// headers it claims arrives short and the protocols of `madrust_net` refuse
    /// it. The buffer of a frame goes back into the receive queue as soon as the
    /// handler returns, because the device cannot receive into a queue that holds
    /// no buffer.
    ///
    /// Returns the number of entries read; zero means that nothing arrived.
    pub fn poll(&mut self, mut handler: impl FnMut(&[u8])) -> Result<usize, Error> {
        let used = used_offset(self.rx.descriptors);
        // SAFETY: the index of the used ring lies inside the ring block, and the
        // read is volatile so the caller sees the word the device writes.
        let available =
            unsafe { core::ptr::read_volatile(slot::<u16>(self.rx.base, used + USED_INDEX)) };

        let mut read = 0;
        while self.rx_used != available {
            // The index of the used ring counts completions, not slots, so the
            // entry wraps with the size of the ring.
            let entry = usize::from(self.rx_used % self.rx.descriptors);
            let offset = used + USED_HEADER + entry * USED_ENTRY;

            // SAFETY: the entry lies inside the used ring, which lies inside the
            // ring block the driver owns.
            let (descriptor, moved) = unsafe {
                (
                    core::ptr::read_volatile(slot::<u32>(self.rx.base, offset + USED_ID)),
                    core::ptr::read_volatile(slot::<u32>(self.rx.base, offset + USED_LENGTH)),
                )
            };
            self.rx_used = self.rx_used.wrapping_add(1);
            read += 1;

            if descriptor >= u32::from(self.rx.descriptors) {
                crate::kprintln!(
                    "[virtio] rx queue={} desc={} len={} ignored: the descriptor is not one of the queue",
                    QUEUE_RX,
                    descriptor,
                    moved
                );
                continue;
            }

            let length = moved as usize;
            if length > FRAME_SIZE {
                crate::kprintln!(
                    "[virtio] rx queue={} desc={} len={} ignored: longer than the buffer of its descriptor",
                    QUEUE_RX,
                    descriptor,
                    moved
                );
                continue;
            }

            // SAFETY: the descriptor index is inside the descriptor array, and its
            // first field is the address of the buffer it names.
            let buffer = unsafe {
                core::ptr::read_volatile(slot::<u64>(
                    self.rx.base,
                    descriptor as usize * DESC_SIZE + DESC_ADDRESS,
                ))
            };

            if length >= NET_HEADER {
                // SAFETY: the buffer is a page the driver posted, the device wrote
                // `length` bytes into it as reported in the used ring, the frame
                // starts after the header of the device, and `length` is at most the
                // size of the page, so the slice stays inside it.
                let frame = unsafe {
                    core::slice::from_raw_parts(slot::<u8>(buffer, NET_HEADER), length - NET_HEADER)
                };
                handler(frame);
            } else {
                crate::kprintln!(
                    "[virtio] rx queue={} desc={} len={} ignored: shorter than the header of the device",
                    QUEUE_RX,
                    descriptor,
                    moved
                );
            }

            self.repost(descriptor);
        }
        Ok(read)
    }

    /// Give the buffer of `descriptor` back to the receive queue.
    ///
    /// The device takes no work until the index of the available ring is written,
    /// so one buffer is enough to keep the queue alive for the next frame.
    fn repost(&mut self, descriptor: u32) {
        let avail = avail_offset(self.rx.descriptors);
        let entry = usize::from(self.rx_avail % self.rx.descriptors);

        // SAFETY: entry `entry` of the available ring lies inside the ring block,
        // and the descriptor it names is inside the descriptor array.
        unsafe {
            core::ptr::write_volatile(
                slot::<u16>(self.rx.base, avail + AVAIL_HEADER + entry * AVAIL_ENTRY),
                descriptor as u16,
            );
        }
        fence(Ordering::SeqCst);

        self.rx_avail = self.rx_avail.wrapping_add(1);
        // SAFETY: the index of the available ring lies inside the ring block.
        unsafe {
            core::ptr::write_volatile(
                slot::<u16>(self.rx.base, avail + AVAIL_INDEX),
                self.rx_avail,
            );
        }
        fence(Ordering::SeqCst);

        outw(BAR_BASE + QUEUE_NOTIFY, QUEUE_RX);
    }
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
