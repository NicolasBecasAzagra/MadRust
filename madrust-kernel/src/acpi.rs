// SPDX-License-Identifier: GPL-2.0-only
//! ACPI discovery of the processors (milestone E3b).
//!
//! The Xen PVH handover carries the physical address of the RSDP, the root of
//! the ACPI tables. This module validates the RSDP, walks the RSDT or XSDT and
//! parses the MADT: the enabled local APICs, the local APIC base and the input
//! output APIC. It reads the tables through the direct map and never allocates,
//! so it can run before the heap exists.
//!
//! A machine without a usable table is not an error. The caller stays on the
//! boot processor and prints why.

use madrust_mm::{phys_to_virt, DIRECT_MAP_SPAN};

use crate::percpu::MAX_CPUS;

/// Size of one standard ACPI table header.
const HEADER_SIZE: usize = 36;
/// Offset of the local APIC address inside the MADT.
const MADT_LAPIC_ADDRESS: usize = 36;
/// Offset of the first MADT entry.
const MADT_ENTRIES: usize = 44;
/// Largest table the parser accepts, so a corrupt length cannot walk memory.
const MAX_TABLE_SIZE: usize = 1024 * 1024;
/// Lowest address of the BIOS ROM area that may hold the RSDP.
const BIOS_ROM_START: u64 = 0xE_0000;
/// End of the BIOS ROM area.
const BIOS_ROM_END: u64 = 0x10_0000;
/// Length of the RSDP area at the start of the extended BIOS data area.
const EBDA_SCAN_SIZE: u64 = 1024;
/// Address of the extended BIOS data area segment pointer in low memory.
const EBDA_POINTER: u64 = 0x40E;

/// Input output APIC described by the MADT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApic {
    /// Identifier from the MADT, reported in the boot log.
    pub id: u8,
    /// Physical address of the memory mapped register window.
    pub address: u64,
    /// First global system interrupt this APIC handles.
    pub gsi_base: u32,
}

/// What the firmware says about the interrupt hardware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// Enabled local APICs, in table order.
    pub processors: [u32; MAX_CPUS],
    /// Number of entries in [`Info::processors`].
    pub processor_count: usize,
    /// Enabled processors the table described, including ones beyond the
    /// kernel bound.
    pub described_processors: usize,
    /// Physical address of the local APIC register window.
    pub lapic_address: u64,
    /// The input output APIC, when the table describes one.
    pub ioapic: Option<IoApic>,
    /// Global system interrupt the timer line is routed to.
    pub timer_gsi: u32,
    /// MPS INTI flags of the timer interrupt source override.
    pub timer_flags: u16,
}

/// Why the interrupt hardware could not be discovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The handover carried no RSDP and the legacy areas do not contain one.
    NoRsdp,
    /// The RSDP signature or checksum is wrong.
    BadRsdp,
    /// The RSDT or XSDT signature or checksum is wrong.
    BadRootTable,
    /// The root table does not contain a MADT.
    NoMadt,
    /// The MADT signature or checksum is wrong.
    BadMadt,
    /// A table points outside the direct map or past its own length.
    Truncated,
}

/// Read a physical byte range through the direct map.
fn bytes(physical: u64, length: usize) -> Result<&'static [u8], Error> {
    let end = physical
        .checked_add(length as u64)
        .ok_or(Error::Truncated)?;
    if end > DIRECT_MAP_SPAN {
        return Err(Error::Truncated);
    }
    // SAFETY: the range ends below the direct map span and the ACPI tables are
    // resident RAM the firmware left intact for the life of the boot. The
    // slice is read only and no other reference to the same bytes is created
    // while it lives.
    Ok(unsafe { core::slice::from_raw_parts(phys_to_virt(physical) as *const u8, length) })
}

/// Read one unaligned little endian `u16`.
fn u16_at(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// Read one unaligned little endian `u32`.
fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Read one unaligned little endian `u64`.
fn u64_at(data: &[u8], offset: usize) -> Option<u64> {
    let bytes = data.get(offset..offset + 8)?;
    let mut value = [0u8; 8];
    value.copy_from_slice(bytes);
    Some(u64::from_le_bytes(value))
}

/// Check an ACPI checksum: the bytes of the table sum to zero.
fn checksum(data: &[u8]) -> bool {
    data.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

/// Validate one ACPI table header and return its length.
fn table_length(physical: u64, signature: &[u8; 4]) -> Result<usize, Error> {
    let header = bytes(physical, HEADER_SIZE)?;
    if &header[0..4] != signature {
        return Err(Error::Truncated);
    }
    let length = u32_at(header, 4).ok_or(Error::Truncated)? as usize;
    if !(HEADER_SIZE..=MAX_TABLE_SIZE).contains(&length) {
        return Err(Error::Truncated);
    }
    let table = bytes(physical, length)?;
    if !checksum(table) {
        return Err(Error::Truncated);
    }
    Ok(length)
}

/// Validate a RSDP at `physical`.
fn rsdp(physical: u64) -> Result<Info, Error> {
    let base = bytes(physical, 24)?;
    if &base[0..8] != b"RSD PTR " || !checksum(&base[..20]) {
        return Err(Error::BadRsdp);
    }
    let revision = base[15];
    let root = if revision >= 2 {
        let length = u32_at(base, 20).ok_or(Error::BadRsdp)? as usize;
        if !(HEADER_SIZE..=MAX_TABLE_SIZE).contains(&length) {
            return Err(Error::BadRsdp);
        }
        let extended = bytes(physical, length)?;
        if !checksum(extended) {
            return Err(Error::BadRsdp);
        }
        let xsdt = u64_at(extended, 24).ok_or(Error::BadRsdp)?;
        if xsdt == 0 {
            u64::from(u32_at(extended, 16).ok_or(Error::BadRsdp)?)
        } else {
            xsdt
        }
    } else {
        u64::from(u32_at(base, 16).ok_or(Error::BadRsdp)?)
    };
    root_table(root)
}

/// Walk the RSDT or XSDT and parse its MADT.
fn root_table(physical: u64) -> Result<Info, Error> {
    if physical == 0 {
        return Err(Error::BadRootTable);
    }
    let header = bytes(physical, HEADER_SIZE)?;
    let xsdt = &header[0..4] == b"XSDT";
    if !xsdt && &header[0..4] != b"RSDT" {
        return Err(Error::BadRootTable);
    }
    let signature: &[u8; 4] = if xsdt { b"XSDT" } else { b"RSDT" };
    let length = table_length(physical, signature)?;
    let entry_size = if xsdt { 8 } else { 4 };
    let entries = (length - HEADER_SIZE) / entry_size;

    for index in 0..entries {
        let offset = HEADER_SIZE + index * entry_size;
        let child = if xsdt {
            u64_at(bytes(physical, length)?, offset).ok_or(Error::Truncated)?
        } else {
            u64::from(u32_at(bytes(physical, length)?, offset).ok_or(Error::Truncated)?)
        };
        if child == 0 {
            continue;
        }
        let child_header = bytes(child, HEADER_SIZE)?;
        if &child_header[0..4] == b"APIC" {
            return madt(child);
        }
    }
    Err(Error::NoMadt)
}

/// Parse the MADT.
fn madt(physical: u64) -> Result<Info, Error> {
    let length = table_length(physical, b"APIC").map_err(|_| Error::BadMadt)?;
    let table = bytes(physical, length)?;
    let lapic_address = u64::from(u32_at(table, MADT_LAPIC_ADDRESS).ok_or(Error::BadMadt)?);
    let mut info = Info {
        processors: [0; MAX_CPUS],
        processor_count: 0,
        described_processors: 0,
        lapic_address,
        ioapic: None,
        timer_gsi: 0,
        timer_flags: 0,
    };

    let mut offset = MADT_ENTRIES;
    while offset + 2 <= length {
        let kind = table[offset];
        let entry_length = table[offset + 1] as usize;
        if entry_length < 2 || offset + entry_length > length {
            return Err(Error::BadMadt);
        }
        let entry = &table[offset..offset + entry_length];
        match kind {
            // Local APIC.
            0 if entry_length >= 8 => {
                let flags = u32_at(entry, 4).ok_or(Error::BadMadt)?;
                let apic_id = entry[3];
                accept(&mut info, u32::from(apic_id), flags);
            }
            // Input output APIC.
            1 if entry_length >= 12 => {
                if info.ioapic.is_none() {
                    info.ioapic = Some(IoApic {
                        id: entry[2],
                        address: u64::from(u32_at(entry, 4).ok_or(Error::BadMadt)?),
                        gsi_base: u32_at(entry, 8).ok_or(Error::BadMadt)?,
                    });
                }
            }
            // Interrupt source override: the timer line is source zero.
            2 if entry_length >= 10 => {
                if entry[2] == 0 && entry[3] == 0 {
                    info.timer_gsi = u32_at(entry, 4).ok_or(Error::BadMadt)?;
                    info.timer_flags = u16_at(entry, 8).ok_or(Error::BadMadt)?;
                }
            }
            // Local APIC address override.
            5 if entry_length >= 16 => {
                info.lapic_address = u64_at(entry, 4).ok_or(Error::BadMadt)?;
            }
            // x2APIC: use it only when the identifier still fits the xAPIC
            // destination field of the interrupt command register.
            9 if entry_length >= 16 => {
                let flags = u32_at(entry, 8).ok_or(Error::BadMadt)?;
                let apic_id = u32_at(entry, 4).ok_or(Error::BadMadt)?;
                accept(&mut info, apic_id, flags);
            }
            _ => {}
        }
        offset += entry_length;
    }

    if info.described_processors == 0 {
        return Err(Error::BadMadt);
    }
    Ok(info)
}

/// Record one local APIC if the firmware says it is usable.
///
/// The xAPIC destination field of the ICR is eight bits wide, so an identifier
/// above `0xFF` cannot be addressed by the startup sequence and is not counted
/// as a processor this kernel can start.
fn accept(info: &mut Info, apic_id: u32, flags: u32) {
    if flags & 1 == 0 || apic_id > 0xFF {
        return;
    }
    info.described_processors += 1;
    if info.processor_count < MAX_CPUS {
        info.processors[info.processor_count] = apic_id;
        info.processor_count += 1;
    }
}

/// Find the RSDP of the machine.
///
/// The PVH handover usually carries it. When it does not, the two areas the
/// PC firmware keeps one in are scanned, which keeps the kernel booting on a
/// loader that does not fill the field.
pub fn discover(rsdp_paddr: u64) -> Result<Info, Error> {
    if rsdp_paddr != 0 {
        return rsdp(rsdp_paddr);
    }
    if let Ok(info) = scan_ebda() {
        return Ok(info);
    }
    scan_rom()
}

/// Scan the extended BIOS data area for an RSDP.
fn scan_ebda() -> Result<Info, Error> {
    let pointer = bytes(EBDA_POINTER, 2)?;
    let segment = u64::from(u16::from_le_bytes([pointer[0], pointer[1]]));
    let base = segment << 4;
    if base == 0 {
        return Err(Error::NoRsdp);
    }
    if let Some(r#match) = find_rsdp(base, EBDA_SCAN_SIZE) {
        return rsdp(r#match);
    }
    Err(Error::NoRsdp)
}

/// Scan the BIOS ROM area for an RSDP.
fn scan_rom() -> Result<Info, Error> {
    find_rsdp(BIOS_ROM_START, BIOS_ROM_END - BIOS_ROM_START)
        .map(rsdp)
        .unwrap_or(Err(Error::NoRsdp))
}

/// Find the eight byte RSDP signature on a 16 byte grid.
fn find_rsdp(start: u64, length: u64) -> Option<u64> {
    let mut offset = 0;
    while offset < length {
        let address = start + offset;
        if let Ok(window) = bytes(address, 8) {
            if window == b"RSD PTR " {
                return Some(address);
            }
        }
        offset += 16;
    }
    None
}
