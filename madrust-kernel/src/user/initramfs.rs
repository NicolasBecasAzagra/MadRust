// SPDX-License-Identifier: GPL-2.0-only
//! Initramfs discovery and program selection.
//!
//! QEMU hands the `-initrd` file to a PVH kernel as a module: the start info
//! names a list of [`HvmModule`] records and one of them describes the archive
//! bytes. The archive is an uncompressed `newc` `cpio` file, which mainline
//! Linux accepts too, so both kernels receive exactly the same bytes.
//!
//! The module bytes live in RAM that the frame allocator keeps reserved for the
//! whole boot (see `memory::init`), so the slices handed out here stay valid
//! until the program image has been copied into the user half.

use madrust_bootinfo::HvmModule;
use madrust_initramfs::Archive;
use madrust_mm::phys_to_virt;

/// One program found in the archive.
pub struct ProgramFile {
    /// Normalized path without a leading slash, for the boot log.
    pub name: &'static str,
    /// File contents, borrowed from the reserved module memory.
    pub data: &'static [u8],
}

/// The archive found in the PVH module list.
pub struct Initramfs {
    module_count: usize,
    archive: Archive<'static>,
}

impl Initramfs {
    /// Number of modules the loader passed.
    pub const fn module_count(&self) -> usize {
        self.module_count
    }

    /// Number of regular files in the archive.
    pub fn file_count(&self) -> usize {
        self.archive.file_count()
    }

    /// The program to run: `/bench` when present, otherwise the first
    /// executable file.
    pub fn program(&self) -> Option<ProgramFile> {
        let entry = self.archive.find_program()?;
        Some(ProgramFile {
            name: entry.normalized_name().unwrap_or("program"),
            data: entry.data(),
        })
    }
}

/// Find the archive among the loader's modules.
///
/// The initramfs is the module that starts with a `cpio` `newc` magic. When
/// there is exactly one module and it does not carry the magic, it is still
/// reported, so the boot log names the file the loader handed over instead of
/// silently ignoring it.
pub fn find(module_count: usize, modules: &[HvmModule]) -> Option<Initramfs> {
    let bytes = modules
        .iter()
        .filter(|module| module.size != 0)
        .map(module_bytes)
        .find(|bytes| is_cpio(bytes))
        .or_else(|| {
            if modules.len() == 1 && modules[0].size != 0 {
                Some(module_bytes(&modules[0]))
            } else {
                None
            }
        })?;

    let archive = Archive::parse(bytes).ok()?;
    Some(Initramfs {
        module_count,
        archive,
    })
}

/// Borrow the bytes of one module through the direct map.
fn module_bytes(module: &HvmModule) -> &'static [u8] {
    let Ok(size) = usize::try_from(module.size) else {
        return &[];
    };
    // SAFETY: the PVH loader points `paddr` at `size` consecutive bytes in RAM.
    // `memory::init` reserves every module range before any frame is handed out,
    // so the bytes stay untouched for the whole boot, and the direct map
    // translates the physical address.
    unsafe { core::slice::from_raw_parts(phys_to_virt(module.paddr) as *const u8, size) }
}

/// `true` when `bytes` starts with one of the `newc` magics.
fn is_cpio(bytes: &[u8]) -> bool {
    bytes.starts_with(madrust_initramfs::MAGIC_NEWC)
        || bytes.starts_with(madrust_initramfs::MAGIC_CRC)
}
