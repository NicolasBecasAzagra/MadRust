// SPDX-License-Identifier: GPL-2.0-only
//! Parsing of the Xen PVH `hvm_start_info` structure for MadRust.
//!
//! This crate is intentionally free of hardware access and `unsafe` so it can
//! be unit-tested on the host.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]
#![forbid(unsafe_code)]

/// Magic value of `struct hvm_start_info` (`HVM_START_MAGIC`).
pub const HVM_START_MAGIC: u32 = 0x336e_c578;

/// Only version 1 of the PVH start info ABI is supported.
pub const HVM_START_INFO_VERSION: u32 = 1;

/// Layout of `struct hvm_start_info` from the Xen PVH ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HvmStartInfo {
    pub magic: u32,
    pub version: u32,
    pub flags: u32,
    pub nr_modules: u32,
    pub modlist_paddr: u64,
    pub cmdline_paddr: u64,
    pub rsdp_paddr: u64,
    pub memmap_paddr: u64,
    pub memmap_entries: u32,
    pub reserved: u32,
}

/// One entry of the PVH physical memory map.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HvmMemmapEntry {
    pub addr: u64,
    pub size: u64,
    pub kind: u32,
    pub reserved: u32,
}

/// One entry of the PVH module list.
///
/// A direct kernel boot hands the `-initrd` file over as a module: `paddr` and
/// `size` describe the bytes in RAM and `cmdline_paddr` points at an optional
/// NUL terminated string. QEMU names the initrd module `initrd`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HvmModule {
    pub paddr: u64,
    pub size: u64,
    pub cmdline_paddr: u64,
    pub reserved: u64,
}

impl HvmModule {
    /// End address (exclusive) of the module bytes, saturating on overflow.
    pub const fn end(&self) -> u64 {
        self.paddr.saturating_add(self.size)
    }
}

/// Memory map entry types defined by the PVH ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum MemmapKind {
    Ram = 1,
    Reserved = 2,
    Acpi = 3,
    Nvs = 4,
    Unusable = 5,
    Disabled = 6,
    Pmem = 7,
}

impl MemmapKind {
    /// Convert a raw PVH memory map type into a [`MemmapKind`].
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Ram),
            2 => Some(Self::Reserved),
            3 => Some(Self::Acpi),
            4 => Some(Self::Nvs),
            5 => Some(Self::Unusable),
            6 => Some(Self::Disabled),
            7 => Some(Self::Pmem),
            _ => None,
        }
    }
}

impl HvmMemmapEntry {
    /// Decoded kind of this entry, or `None` for an unknown type.
    pub const fn kind(&self) -> Option<MemmapKind> {
        MemmapKind::from_raw(self.kind)
    }

    /// `true` when this entry describes usable RAM.
    pub const fn is_ram(&self) -> bool {
        matches!(self.kind(), Some(MemmapKind::Ram))
    }

    /// End address (exclusive) of this entry, saturating on overflow.
    pub const fn end(&self) -> u64 {
        self.addr.saturating_add(self.size)
    }
}

/// Reasons why a start info block is rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The magic field does not contain [`HVM_START_MAGIC`].
    BadMagic { found: u32 },
    /// The PVH ABI version is not supported by this kernel.
    UnsupportedVersion(u32),
    /// The number of provided memory map entries does not match the header.
    MemmapCountMismatch { declared: u32, provided: usize },
    /// The number of provided module list entries does not match the header.
    ModuleCountMismatch { declared: u32, provided: usize },
}

/// A validated view over the PVH boot information.
#[derive(Clone, Copy, Debug)]
pub struct BootInfo<'a> {
    start: &'a HvmStartInfo,
    memmap: &'a [HvmMemmapEntry],
    modules: &'a [HvmModule],
}

impl<'a> BootInfo<'a> {
    /// Validate `start` and pair it with the already-mapped memory map and
    /// module list.
    pub fn parse(
        start: &'a HvmStartInfo,
        memmap: &'a [HvmMemmapEntry],
        modules: &'a [HvmModule],
    ) -> Result<Self, ParseError> {
        if start.magic != HVM_START_MAGIC {
            return Err(ParseError::BadMagic { found: start.magic });
        }
        if start.version != HVM_START_INFO_VERSION {
            return Err(ParseError::UnsupportedVersion(start.version));
        }
        if start.memmap_entries as usize != memmap.len() {
            return Err(ParseError::MemmapCountMismatch {
                declared: start.memmap_entries,
                provided: memmap.len(),
            });
        }
        if start.nr_modules as usize != modules.len() {
            return Err(ParseError::ModuleCountMismatch {
                declared: start.nr_modules,
                provided: modules.len(),
            });
        }
        Ok(Self {
            start,
            memmap,
            modules,
        })
    }

    /// The raw start info header.
    pub const fn start_info(&self) -> &'a HvmStartInfo {
        self.start
    }

    /// The full physical memory map.
    pub const fn memmap(&self) -> &'a [HvmMemmapEntry] {
        self.memmap
    }

    /// The modules the loader passed, in the order it listed them.
    pub const fn modules(&self) -> &'a [HvmModule] {
        self.modules
    }

    /// Iterator over the usable RAM regions.
    pub fn ram_regions(&self) -> impl Iterator<Item = &'a HvmMemmapEntry> + '_ {
        self.memmap.iter().filter(|entry| entry.is_ram())
    }

    /// Total size of usable RAM in bytes.
    pub fn total_ram(&self) -> u64 {
        self.ram_regions().map(|entry| entry.size).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_info(memmap_entries: u32) -> HvmStartInfo {
        HvmStartInfo {
            magic: HVM_START_MAGIC,
            version: HVM_START_INFO_VERSION,
            flags: 0,
            nr_modules: 0,
            modlist_paddr: 0,
            cmdline_paddr: 0,
            rsdp_paddr: 0,
            memmap_paddr: 0x40,
            memmap_entries,
            reserved: 0,
        }
    }

    fn entry(addr: u64, size: u64, kind: u32) -> HvmMemmapEntry {
        HvmMemmapEntry {
            addr,
            size,
            kind,
            reserved: 0,
        }
    }

    fn module(paddr: u64, size: u64, cmdline_paddr: u64) -> HvmModule {
        HvmModule {
            paddr,
            size,
            cmdline_paddr,
            reserved: 0,
        }
    }

    #[test]
    fn layout_matches_pvh_abi() {
        assert_eq!(core::mem::size_of::<HvmStartInfo>(), 56);
        assert_eq!(core::mem::size_of::<HvmMemmapEntry>(), 24);
        assert_eq!(core::mem::size_of::<HvmModule>(), 32);
        assert_eq!(core::mem::align_of::<HvmStartInfo>(), 8);
        assert_eq!(core::mem::align_of::<HvmMemmapEntry>(), 8);
        assert_eq!(core::mem::align_of::<HvmModule>(), 8);
    }

    #[test]
    fn exposes_the_module_list() {
        let mut start = start_info(0);
        start.nr_modules = 1;
        start.modlist_paddr = 0x80_0000;
        let modules = [module(0x10_0000, 0x400, 0x20_0000)];
        let info = BootInfo::parse(&start, &[], &modules).expect("valid module list");
        assert_eq!(info.modules(), &modules);
        assert_eq!(info.modules()[0].end(), 0x10_0400);
    }

    #[test]
    fn rejects_a_module_count_mismatch() {
        let mut start = start_info(0);
        start.nr_modules = 2;
        let modules = [module(0x10_0000, 0x400, 0)];
        let err = BootInfo::parse(&start, &[], &modules).unwrap_err();
        assert_eq!(
            err,
            ParseError::ModuleCountMismatch {
                declared: 2,
                provided: 1
            }
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut start = start_info(0);
        start.magic = 0xdead_beef;
        let err = BootInfo::parse(&start, &[], &[]).unwrap_err();
        assert_eq!(err, ParseError::BadMagic { found: 0xdead_beef });
    }

    #[test]
    fn rejects_unknown_version() {
        let mut start = start_info(0);
        start.version = 2;
        let err = BootInfo::parse(&start, &[], &[]).unwrap_err();
        assert_eq!(err, ParseError::UnsupportedVersion(2));
    }

    #[test]
    fn accepts_empty_memmap() {
        let start = start_info(0);
        let info = BootInfo::parse(&start, &[], &[]).expect("empty memmap is valid");
        assert!(info.memmap().is_empty());
        assert_eq!(info.total_ram(), 0);
    }

    #[test]
    fn parses_memmap_and_totals_ram() {
        let start = start_info(4);
        let entries = [
            entry(0x0, 0x9_fc00, 1),
            entry(0x9_fc00, 0x400, 2),
            entry(0x10_0000, 0x1f0_0000, 1),
            entry(0x1_0000_0000, 0x1000_0000, 3),
        ];
        let info = BootInfo::parse(&start, &entries, &[]).expect("valid memmap");
        assert_eq!(info.memmap(), &entries);
        assert_eq!(info.ram_regions().count(), 2);
        assert_eq!(info.total_ram(), 0x9_fc00 + 0x1f0_0000);
        assert_eq!(info.memmap()[3].kind(), Some(MemmapKind::Acpi));
        assert_eq!(info.memmap()[0].end(), 0x9_fc00);
    }

    #[test]
    fn rejects_memmap_count_mismatch() {
        let start = start_info(2);
        let entries = [entry(0x0, 0x10_0000, 1)];
        let err = BootInfo::parse(&start, &entries, &[]).unwrap_err();
        assert_eq!(
            err,
            ParseError::MemmapCountMismatch {
                declared: 2,
                provided: 1
            }
        );
    }

    #[test]
    fn unknown_memmap_kind_is_not_ram() {
        let weird = entry(0x0, 0x1000, 42);
        assert_eq!(weird.kind(), None);
        assert!(!weird.is_ram());
        assert_eq!(weird.end(), 0x1000);
    }
}
