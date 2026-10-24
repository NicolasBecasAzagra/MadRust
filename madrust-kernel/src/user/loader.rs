// SPDX-License-Identifier: GPL-2.0-only
//! ELF64 program loading into the user half.
//!
//! The loader walks the `PT_LOAD` segments of the embedded image, allocates one
//! frame per page of the segment, copies the file backed bytes, leaves the rest
//! zeroed (the `p_memsz` minus `p_filesz` tail), and installs the page in the
//! user half with [`super::paging::map_page`]. Pages shared by two segments are
//! allocated once: the second segment reuses the frame already mapped there.
//!
//! Two image kinds are supported. An `ET_EXEC` image carries absolute
//! addresses and is loaded where it was linked. An `ET_DYN` image, which is
//! what a statically linked musl binary is, carries offsets from a load base
//! of zero: every `p_vaddr` is shifted by [`LOAD_BASE`] and the program enters
//! at `LOAD_BASE + e_entry`.
//!
//! `AT_PHDR` must name the program header table of the running image, because
//! the musl startup code derives the load base and the address of the `PT_TLS`
//! image from it. For `ET_DYN` the table is already inside the first loadable
//! segment, so the loader reports that address. In an `ET_EXEC` image the
//! table sits outside every segment, so it is copied into one dedicated user
//! page instead.

use core::ptr::copy_nonoverlapping;

use madrust_elf::{Elf, ElfError, ProgramHeader, USER_SPACE_END};
use madrust_mm::paging::PAGE_SIZE;
use madrust_mm::phys_to_virt;

use super::paging;

/// Address a position independent image is loaded at.
///
/// The same value the previous `ET_EXEC` images were linked at, so the log and
/// the debugger keep showing the same picture. The image is well below the
/// anonymous mapping area at 256 MiB.
pub const LOAD_BASE: u64 = 0x0040_0000;

/// User address of the page that holds the copied program header table.
///
/// It sits below the stack at 2 GiB and above the program at 4 MiB, so it
/// cannot collide with either. Only an `ET_EXEC` image needs the copy: the
/// table of a position independent image is mapped by its first segment.
const PROGRAM_HEADER_PAGE: u64 = 0x0000_0000_6000_0000;

/// A loaded program, ready to be entered.
pub struct Image {
    load_base: u64,
    entry: u64,
    program_headers: u64,
    program_header_entries: u16,
    program_header_size: u16,
    break_address: u64,
}

impl Image {
    /// Address the image was loaded at.
    ///
    /// Zero for an `ET_EXEC` image, whose addresses are absolute, and
    /// [`LOAD_BASE`] for a position independent one.
    pub const fn load_base(&self) -> u64 {
        self.load_base
    }

    /// Entry point the CPU must jump to, with the load base applied.
    pub const fn entry(&self) -> u64 {
        self.entry
    }

    /// Address of the copied program header table, for `AT_PHDR`.
    pub const fn program_headers(&self) -> u64 {
        self.program_headers
    }

    /// Number of program headers, for `AT_PHNUM`.
    pub const fn program_header_entries(&self) -> u16 {
        self.program_header_entries
    }

    /// Size of one program header, for `AT_PHENT`.
    pub const fn program_header_size(&self) -> u16 {
        self.program_header_size
    }

    /// First address past the loaded image, the initial program break.
    pub const fn break_address(&self) -> u64 {
        self.break_address
    }
}

/// Reason why an image could not be loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// The ELF headers are malformed or describe a file this loader cannot run.
    Elf(ElfError),
    /// The image has no `PT_LOAD` segment with a non-zero memory size.
    NoLoadableSegment,
    /// A frame for a program page or a page table page is unavailable.
    OutOfMemory,
    /// The program header table does not fit in one page.
    ProgramHeadersTooLarge,
}

/// Load every `PT_LOAD` segment of `bytes` into the user half.
pub fn load(bytes: &[u8]) -> Result<Image, LoadError> {
    let elf = Elf::parse(bytes).map_err(LoadError::Elf)?;
    let base = if elf.is_position_independent() {
        LOAD_BASE
    } else {
        0
    };

    let mut break_address = 0;
    let mut loads = 0;
    for (index, header) in elf.program_headers().enumerate() {
        if !header.is_load() || header.memsz == 0 {
            continue;
        }
        // The parser checked the segment against the user half without the
        // base, so the shifted end has to be checked again.
        let end = base
            .checked_add(header.end())
            .filter(|end| *end <= USER_SPACE_END)
            .ok_or(LoadError::Elf(ElfError::SegmentOutsideUserSpace { index }))?;
        load_segment(bytes, &header, base)?;
        break_address = break_address.max(end);
        loads += 1;
    }
    if loads == 0 {
        return Err(LoadError::NoLoadableSegment);
    }

    let program_headers = if elf.is_position_independent() {
        // The table is mapped by the first segment. Reporting a copy would
        // make the musl startup code derive the wrong load base for `PT_TLS`.
        match elf.program_header_address() {
            Some(address) => address + base,
            None => return Err(LoadError::Elf(ElfError::ProgramHeadersNotMapped)),
        }
    } else {
        copy_program_headers(bytes, &elf)?
    };

    let entry =
        elf.entry_with_base(base)
            .ok_or(LoadError::Elf(ElfError::EntryOutsideUserSpace {
                entry: elf.entry(),
            }))?;

    Ok(Image {
        load_base: base,
        entry,
        program_headers,
        program_header_entries: elf.program_header_count(),
        program_header_size: elf.program_header_size(),
        break_address,
    })
}

/// Map and fill the pages of one loadable segment at `base`.
fn load_segment(bytes: &[u8], header: &ProgramHeader, base: u64) -> Result<(), LoadError> {
    let vaddr = header.vaddr + base;
    let first = vaddr & !(PAGE_SIZE - 1);
    let end = vaddr + header.memsz;
    let file_end = vaddr + header.filesz;

    let mut address = first;
    while address < end {
        let physical = match paging::frame_at(address) {
            Some(existing) => existing,
            None => {
                let frame = crate::memory::with(|manager| manager.alloc_frame())
                    .flatten()
                    .ok_or(LoadError::OutOfMemory)?;
                // SAFETY: the allocator just handed this frame out and nothing
                // else references it, so the zero fill is exclusive. The direct
                // map translates the physical address.
                unsafe {
                    core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE as usize);
                }
                paging::map_page(address, frame, true).ok_or(LoadError::OutOfMemory)?;
                frame
            }
        };

        // Copy the part of this page that the file backs. A page shared by two
        // segments is copied twice at most, always from that segment's bytes.
        let copy_start = address.max(vaddr);
        let copy_end = (address + PAGE_SIZE).min(file_end);
        if copy_start < copy_end {
            let offset = header.offset + (copy_start - vaddr);
            let length = usize::try_from(copy_end - copy_start)
                .map_err(|_| LoadError::Elf(ElfError::SegmentFileRangeOutsideImage { index: 0 }))?;
            let start = usize::try_from(offset)
                .map_err(|_| LoadError::Elf(ElfError::SegmentFileRangeOutsideImage { index: 0 }))?;
            let source = bytes.get(start..start + length).ok_or(LoadError::Elf(
                ElfError::SegmentFileRangeOutsideImage { index: 0 },
            ))?;

            // SAFETY: `copy_start` lies in the page mapped by `physical`, the
            // offset inside the page is below PAGE_SIZE, and the destination
            // range stays inside that page. The source range was bounds
            // checked against the image.
            unsafe {
                copy_nonoverlapping(
                    source.as_ptr(),
                    phys_to_virt(physical + (copy_start - address)) as *mut u8,
                    length,
                );
            }
        }

        address += PAGE_SIZE;
    }
    Ok(())
}

/// Copy the program header table into a user page and return its address.
fn copy_program_headers(bytes: &[u8], elf: &Elf<'_>) -> Result<u64, LoadError> {
    let table_size = u64::from(elf.program_header_count()) * u64::from(elf.program_header_size());
    let table_offset = elf.program_header_offset();
    if table_size == 0 || table_size > PAGE_SIZE {
        return Err(LoadError::ProgramHeadersTooLarge);
    }

    let start = usize::try_from(table_offset)
        .map_err(|_| LoadError::Elf(ElfError::ProgramHeaderTableOutOfBounds))?;
    let length = usize::try_from(table_size).map_err(|_| LoadError::ProgramHeadersTooLarge)?;
    let source = bytes
        .get(start..start + length)
        .ok_or(LoadError::Elf(ElfError::ProgramHeaderTableOutOfBounds))?;

    let frame = crate::memory::with(|manager| manager.alloc_frame())
        .flatten()
        .ok_or(LoadError::OutOfMemory)?;
    // SAFETY: the frame is freshly allocated, so zeroing it and then writing
    // the copied table are exclusive; the direct map translates the address.
    unsafe {
        let destination = phys_to_virt(frame) as *mut u8;
        core::ptr::write_bytes(destination, 0, PAGE_SIZE as usize);
        copy_nonoverlapping(source.as_ptr(), destination, length);
    }
    paging::map_page(PROGRAM_HEADER_PAGE, frame, true).ok_or(LoadError::OutOfMemory)?;
    Ok(PROGRAM_HEADER_PAGE)
}
