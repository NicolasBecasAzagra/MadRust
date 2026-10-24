// SPDX-License-Identifier: GPL-2.0-only
//! ELF64 program loading into the user half.
//!
//! The loader walks the `PT_LOAD` segments of the embedded image, allocates one
//! frame per page of the segment, copies the file backed bytes, leaves the rest
//! zeroed (the `p_memsz` minus `p_filesz` tail), and installs the page in the
//! user half with [`super::paging::map_page`]. Pages shared by two segments are
//! allocated once: the second segment reuses the frame already mapped there.
//!
//! The program header table itself sits outside every loadable segment in the
//! user image, so it is copied into one dedicated user page. That page is what
//! `AT_PHDR` points at.

use core::ptr::copy_nonoverlapping;

use madrust_elf::{Elf, ElfError, ProgramHeader};
use madrust_mm::paging::PAGE_SIZE;
use madrust_mm::phys_to_virt;

use super::paging;

/// User address of the page that holds the copied program header table.
///
/// It sits below the stack at 2 GiB and above the program at 4 MiB, so it
/// cannot collide with either.
const PROGRAM_HEADER_PAGE: u64 = 0x0000_0000_6000_0000;

/// A loaded program, ready to be entered.
pub struct Image {
    entry: u64,
    program_headers: u64,
    program_header_entries: u16,
    program_header_size: u16,
    break_address: u64,
}

impl Image {
    /// Entry point from the ELF header.
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

    let mut break_address = 0;
    let mut loads = 0;
    for header in elf.program_headers() {
        if !header.is_load() || header.memsz == 0 {
            continue;
        }
        load_segment(bytes, &header)?;
        break_address = break_address.max(header.end());
        loads += 1;
    }
    if loads == 0 {
        return Err(LoadError::NoLoadableSegment);
    }

    let program_headers = copy_program_headers(bytes, &elf)?;

    Ok(Image {
        entry: elf.entry(),
        program_headers,
        program_header_entries: elf.program_header_count(),
        program_header_size: elf.program_header_size(),
        break_address,
    })
}

/// Map and fill the pages of one loadable segment.
fn load_segment(bytes: &[u8], header: &ProgramHeader) -> Result<(), LoadError> {
    let first = header.vaddr & !(PAGE_SIZE - 1);
    let end = header.end();
    let file_end = header.vaddr + header.filesz;

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
                paging::map_page(address, frame).ok_or(LoadError::OutOfMemory)?;
                frame
            }
        };

        // Copy the part of this page that the file backs. A page shared by two
        // segments is copied twice at most, always from that segment's bytes.
        let copy_start = address.max(header.vaddr);
        let copy_end = (address + PAGE_SIZE).min(file_end);
        if copy_start < copy_end {
            let offset = header.offset + (copy_start - header.vaddr);
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
    paging::map_page(PROGRAM_HEADER_PAGE, frame).ok_or(LoadError::OutOfMemory)?;
    Ok(PROGRAM_HEADER_PAGE)
}
