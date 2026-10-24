// SPDX-License-Identifier: GPL-2.0-only
//! ELF64 executable parsing for the MadRust user program loader.
//!
//! The kernel loads one freestanding executable, so the parser accepts exactly
//! the shape that loader supports: a 64-bit little endian `ET_EXEC` file for
//! `EM_X86_64` whose `PT_LOAD` segments stay inside the user half. Anything
//! else is rejected with a reason instead of being loaded partially.
//!
//! Nothing here touches hardware or allocates memory: [`Elf::parse`] only
//! borrows the image bytes and every read is bounds checked, so the same code
//! runs in the kernel and in host unit tests.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

/// Size of the ELF64 file header.
pub const EHDR_SIZE: usize = 64;

/// Size of one ELF64 program header.
pub const PHDR_SIZE: u16 = 56;

/// Program header type of a loadable segment.
pub const PT_LOAD: u32 = 1;

/// Object file type of a static executable.
pub const ET_EXEC: u16 = 2;

/// Machine type of x86-64.
pub const EM_X86_64: u16 = 62;

/// First virtual address above the user half.
///
/// The lower canonical half ends at `0x0000_7fff_ffff_ffff`, so the page that
/// starts at [`USER_SPACE_END`] is already outside the address space a ring 3
/// program may reach.
pub const USER_SPACE_END: u64 = 0x0000_8000_0000_0000;

/// Largest program header table the loader accepts.
///
/// The single user program of this milestone has a handful of segments; the
/// bound keeps the table small enough to validate in one go.
pub const MAX_PHDRS: u16 = 64;

/// Reason why an image is not loadable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    /// The image is shorter than the structure being read.
    TooSmall { needed: usize, found: usize },
    /// The first four bytes are not `\x7fELF`.
    BadMagic,
    /// `e_ident[EI_CLASS]` is not `ELFCLASS64`.
    NotElf64,
    /// `e_ident[EI_DATA]` is not `ELFDATA2LSB`.
    NotLittleEndian,
    /// `e_ident[EI_VERSION]` is not the current ELF version.
    UnsupportedIdentVersion { version: u8 },
    /// `e_version` is not the current ELF version.
    UnsupportedVersion { version: u32 },
    /// The file is not a static executable.
    NotExecutable { kind: u16 },
    /// The file was built for another machine.
    WrongMachine { machine: u16 },
    /// `e_phentsize` does not describe an ELF64 program header.
    BadProgramHeaderSize { size: u16 },
    /// The program header table is larger than [`MAX_PHDRS`].
    TooManyProgramHeaders { count: u16 },
    /// The program header table lies outside the image.
    ProgramHeaderTableOutOfBounds,
    /// A program header lies outside the table.
    ProgramHeaderOutOfBounds { index: usize },
    /// A segment carries more file bytes than memory bytes.
    SegmentLargerThanMemory { index: usize },
    /// The file range of a segment lies outside the image.
    SegmentFileRangeOutsideImage { index: usize },
    /// A segment would be mapped outside the user half.
    SegmentOutsideUserSpace { index: usize },
    /// The entry point is outside the user half.
    EntryOutsideUserSpace { entry: u64 },
    /// The program header table is not covered by any loadable segment.
    ProgramHeadersNotMapped,
}

/// One parsed program header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgramHeader {
    /// Segment type, for example [`PT_LOAD`].
    pub kind: u32,
    /// Segment flags: bit 0 execute, bit 1 write, bit 2 read.
    pub flags: u32,
    /// Offset of the segment in the image.
    pub offset: u64,
    /// Virtual address the segment is loaded at.
    pub vaddr: u64,
    /// Physical address hint; ignored by the loader.
    pub paddr: u64,
    /// Bytes copied from the image.
    pub filesz: u64,
    /// Bytes the segment occupies in memory, including the zero fill.
    pub memsz: u64,
    /// Required alignment.
    pub align: u64,
}

impl ProgramHeader {
    /// `true` when the segment must be loaded into memory.
    pub const fn is_load(&self) -> bool {
        self.kind == PT_LOAD
    }

    /// First virtual address past the segment.
    ///
    /// The parser rejects segments whose range does not fit in `u64`, so this
    /// cannot overflow for a header obtained from [`Elf`].
    pub const fn end(&self) -> u64 {
        self.vaddr + self.memsz
    }

    /// First image offset past the file backed part of the segment.
    pub const fn file_end(&self) -> u64 {
        self.offset + self.filesz
    }
}

/// A validated ELF64 executable.
#[derive(Clone, Copy, Debug)]
pub struct Elf<'a> {
    bytes: &'a [u8],
    entry: u64,
    phoff: u64,
    phentsize: u16,
    phnum: u16,
}

/// Read a little endian `u16` at `at`.
fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let end = at.checked_add(2)?;
    let raw = bytes.get(at..end)?;
    Some(u16::from_le_bytes([raw[0], raw[1]]))
}

/// Read a little endian `u32` at `at`.
fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    let raw = bytes.get(at..end)?;
    Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

/// Read a little endian `u64` at `at`.
fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    let end = at.checked_add(8)?;
    let raw = bytes.get(at..end)?;
    Some(u64::from_le_bytes([
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ]))
}

impl<'a> Elf<'a> {
    /// Validate an image and return a view of its headers.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ElfError> {
        if bytes.len() < EHDR_SIZE {
            return Err(ElfError::TooSmall {
                needed: EHDR_SIZE,
                found: bytes.len(),
            });
        }
        if bytes.get(0..4) != Some(b"\x7fELF") {
            return Err(ElfError::BadMagic);
        }
        if bytes[4] != 2 {
            return Err(ElfError::NotElf64);
        }
        if bytes[5] != 1 {
            return Err(ElfError::NotLittleEndian);
        }
        if bytes[6] != 1 {
            return Err(ElfError::UnsupportedIdentVersion { version: bytes[6] });
        }

        let kind = read_u16(bytes, 16).ok_or(ElfError::TooSmall {
            needed: 18,
            found: bytes.len(),
        })?;
        if kind != ET_EXEC {
            return Err(ElfError::NotExecutable { kind });
        }

        let machine = read_u16(bytes, 18).ok_or(ElfError::TooSmall {
            needed: 20,
            found: bytes.len(),
        })?;
        if machine != EM_X86_64 {
            return Err(ElfError::WrongMachine { machine });
        }

        let version = read_u32(bytes, 20).ok_or(ElfError::TooSmall {
            needed: 24,
            found: bytes.len(),
        })?;
        if version != 1 {
            return Err(ElfError::UnsupportedVersion { version });
        }

        let entry = read_u64(bytes, 24).ok_or(ElfError::TooSmall {
            needed: 32,
            found: bytes.len(),
        })?;
        if entry >= USER_SPACE_END {
            return Err(ElfError::EntryOutsideUserSpace { entry });
        }

        let phoff = read_u64(bytes, 32).ok_or(ElfError::TooSmall {
            needed: 40,
            found: bytes.len(),
        })?;
        let phentsize = read_u16(bytes, 54).ok_or(ElfError::TooSmall {
            needed: 56,
            found: bytes.len(),
        })?;
        let phnum = read_u16(bytes, 56).ok_or(ElfError::TooSmall {
            needed: 58,
            found: bytes.len(),
        })?;

        if phentsize != PHDR_SIZE {
            return Err(ElfError::BadProgramHeaderSize { size: phentsize });
        }
        if phnum > MAX_PHDRS {
            return Err(ElfError::TooManyProgramHeaders { count: phnum });
        }

        let table_size = u64::from(phnum) * u64::from(phentsize);
        let table_end = phoff
            .checked_add(table_size)
            .ok_or(ElfError::ProgramHeaderTableOutOfBounds)?;
        if phnum > 0 && table_end > bytes.len() as u64 {
            return Err(ElfError::ProgramHeaderTableOutOfBounds);
        }

        let elf = Self {
            bytes,
            entry,
            phoff,
            phentsize,
            phnum,
        };

        for index in 0..usize::from(phnum) {
            let header = elf
                .program_header(index)
                .ok_or(ElfError::ProgramHeaderOutOfBounds { index })?;
            if header.filesz > header.memsz {
                return Err(ElfError::SegmentLargerThanMemory { index });
            }
            if header.file_end() > bytes.len() as u64 {
                return Err(ElfError::SegmentFileRangeOutsideImage { index });
            }
            if header.end() > USER_SPACE_END || header.vaddr >= USER_SPACE_END {
                return Err(ElfError::SegmentOutsideUserSpace { index });
            }
        }

        Ok(elf)
    }

    /// Entry point from the file header.
    pub const fn entry(&self) -> u64 {
        self.entry
    }

    /// Number of program headers.
    pub const fn program_header_count(&self) -> u16 {
        self.phnum
    }

    /// Size of one program header.
    pub const fn program_header_size(&self) -> u16 {
        self.phentsize
    }

    /// Image offset of the program header table.
    pub const fn program_header_offset(&self) -> u64 {
        self.phoff
    }

    /// The image bytes the headers were parsed from.
    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Parse one program header, or `None` when the index is out of range.
    pub fn program_header(&self, index: usize) -> Option<ProgramHeader> {
        if index >= usize::from(self.phnum) {
            return None;
        }
        let entry = self
            .phoff
            .checked_add((index as u64).checked_mul(u64::from(self.phentsize))?)?;
        let base = usize::try_from(entry).ok()?;
        Some(ProgramHeader {
            kind: read_u32(self.bytes, base)?,
            flags: read_u32(self.bytes, base + 4)?,
            offset: read_u64(self.bytes, base + 8)?,
            vaddr: read_u64(self.bytes, base + 16)?,
            paddr: read_u64(self.bytes, base + 24)?,
            filesz: read_u64(self.bytes, base + 32)?,
            memsz: read_u64(self.bytes, base + 40)?,
            align: read_u64(self.bytes, base + 48)?,
        })
    }

    /// Iterate over every program header.
    pub fn program_headers(&self) -> impl Iterator<Item = ProgramHeader> + '_ {
        (0..usize::from(self.phnum)).filter_map(|index| self.program_header(index))
    }

    /// Virtual address at which the program header table is mapped.
    ///
    /// The loader needs the address for the `AT_PHDR` auxiliary vector entry.
    /// Returns `None` when no loadable segment carries the table, which the
    /// loader reports instead of handing the program a bad pointer.
    pub fn program_header_address(&self) -> Option<u64> {
        let table_size = u64::from(self.phnum) * u64::from(self.phentsize);
        let table_end = self.phoff.checked_add(table_size)?;
        self.program_headers()
            .filter(|header| header.is_load())
            .find_map(|header| {
                let file_end = header.file_end();
                if self.phoff >= header.offset && table_end <= file_end {
                    Some(header.vaddr + (self.phoff - header.offset))
                } else {
                    None
                }
            })
    }

    /// Require that the program header table is mapped.
    pub fn require_program_header_address(&self) -> Result<u64, ElfError> {
        self.program_header_address()
            .ok_or(ElfError::ProgramHeadersNotMapped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHDRS: usize = 2;

    /// Build a minimal valid image: one executable load segment at 0x400000
    /// that also carries the ELF and program headers.
    fn image() -> Vec<u8> {
        let size = 0x400;
        let mut bytes = vec![0u8; size];
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
        bytes[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&0x400000u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&(EHDR_SIZE as u64).to_le_bytes());
        bytes[52..54].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
        bytes[54..56].copy_from_slice(&PHDR_SIZE.to_le_bytes());
        bytes[56..58].copy_from_slice(&(PHDRS as u16).to_le_bytes());

        // First program header: the loadable segment with the code.
        let ph = EHDR_SIZE;
        bytes[ph..ph + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        bytes[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes());
        bytes[ph + 8..ph + 16].copy_from_slice(&0u64.to_le_bytes());
        bytes[ph + 16..ph + 24].copy_from_slice(&0x400000u64.to_le_bytes());
        bytes[ph + 32..ph + 40].copy_from_slice(&(size as u64).to_le_bytes());
        bytes[ph + 40..ph + 48].copy_from_slice(&(size as u64).to_le_bytes());
        bytes
    }

    #[test]
    fn accepts_a_valid_program() {
        let bytes = image();
        let elf = Elf::parse(&bytes).expect("valid image");
        assert_eq!(elf.entry(), 0x400000);
        assert_eq!(elf.program_header_count(), PHDRS as u16);
        let load = elf.program_header(0).expect("first header");
        assert!(load.is_load());
        assert_eq!(load.vaddr, 0x400000);
        assert_eq!(elf.program_header_address(), Some(0x400040));
        assert_eq!(elf.program_headers().count(), PHDRS);
    }

    #[test]
    fn rejects_a_truncated_header() {
        let bytes = image();
        assert_eq!(
            Elf::parse(&bytes[..EHDR_SIZE - 1]).err(),
            Some(ElfError::TooSmall {
                needed: EHDR_SIZE,
                found: EHDR_SIZE - 1
            })
        );
    }

    #[test]
    fn rejects_a_bad_magic() {
        let mut bytes = image();
        bytes[0] = b'X';
        assert_eq!(Elf::parse(&bytes).err(), Some(ElfError::BadMagic));
    }

    #[test]
    fn rejects_a_wrong_machine_type() {
        let mut bytes = image();
        bytes[18..20].copy_from_slice(&EM_X86_64.wrapping_add(1).to_le_bytes());
        assert_eq!(
            Elf::parse(&bytes).err(),
            Some(ElfError::WrongMachine {
                machine: EM_X86_64 + 1
            })
        );
    }

    #[test]
    fn rejects_a_non_executable_file() {
        let mut bytes = image();
        bytes[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        assert_eq!(
            Elf::parse(&bytes).err(),
            Some(ElfError::NotExecutable { kind: 3 })
        );
    }

    #[test]
    fn rejects_a_segment_that_leaves_the_user_half() {
        let mut bytes = image();
        let ph = EHDR_SIZE;
        bytes[ph + 16..ph + 24].copy_from_slice(&(USER_SPACE_END - 0x100).to_le_bytes());
        bytes[ph + 40..ph + 48].copy_from_slice(&0x1000u64.to_le_bytes());
        assert_eq!(
            Elf::parse(&bytes).err(),
            Some(ElfError::SegmentOutsideUserSpace { index: 0 })
        );
    }

    #[test]
    fn rejects_an_entry_outside_the_user_half() {
        let mut bytes = image();
        bytes[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            Elf::parse(&bytes).err(),
            Some(ElfError::EntryOutsideUserSpace { entry: u64::MAX })
        );
    }

    #[test]
    fn rejects_a_segment_past_the_image() {
        let mut bytes = image();
        let ph = EHDR_SIZE;
        bytes[ph + 32..ph + 40].copy_from_slice(&(u64::from(u32::MAX)).to_le_bytes());
        bytes[ph + 40..ph + 48].copy_from_slice(&(u64::from(u32::MAX)).to_le_bytes());
        assert_eq!(
            Elf::parse(&bytes).err(),
            Some(ElfError::SegmentFileRangeOutsideImage { index: 0 })
        );
    }

    #[test]
    fn rejects_a_bad_program_header_size() {
        let mut bytes = image();
        bytes[54..56].copy_from_slice(&32u16.to_le_bytes());
        assert_eq!(
            Elf::parse(&bytes).err(),
            Some(ElfError::BadProgramHeaderSize { size: 32 })
        );
    }

    #[test]
    fn rejects_program_headers_that_are_not_mapped() {
        let mut bytes = image();
        // Move the table into a file range that no segment covers.
        let phoff = 0x300u64;
        bytes[32..40].copy_from_slice(&phoff.to_le_bytes());
        let source = EHDR_SIZE;
        let table = bytes[source..source + PHDRS * usize::from(PHDR_SIZE)].to_vec();
        bytes[phoff as usize..phoff as usize + table.len()].copy_from_slice(&table);
        let ph = phoff as usize;
        bytes[ph + 32..ph + 40].copy_from_slice(&0x100u64.to_le_bytes());
        bytes[ph + 40..ph + 48].copy_from_slice(&0x100u64.to_le_bytes());

        let elf = Elf::parse(&bytes).expect("headers themselves are valid");
        assert_eq!(elf.program_header_address(), None);
        assert_eq!(
            elf.require_program_header_address(),
            Err(ElfError::ProgramHeadersNotMapped)
        );
    }
}
