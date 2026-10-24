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
//!
//! ## The archive as the root file system
//!
//! [`install`] publishes the parsed archive once the boot path has found it.
//! The descriptor table resolves paths through [`resolve`], and every file
//! descriptor borrows the entry data from the module bytes: the kernel never
//! copies a file into the heap. The archive is mapped by the direct map, while
//! the user program runs in the low half of the same page tables, so a read
//! copies straight from the reserved module memory into the user buffer.

use core::cell::UnsafeCell;

use madrust_bootinfo::HvmModule;
use madrust_initramfs::{Archive, Entry, PathError};
use madrust_mm::phys_to_virt;

/// The archive, once the boot path has installed it.
struct ArchiveCell(UnsafeCell<Option<Archive<'static>>>);

// SAFETY: the cell is written exactly once during single-threaded boot, before
// interrupts or user mode are enabled. After that it is only read, and
// `Archive` is `Copy` and immutable.
unsafe impl Sync for ArchiveCell {}

static ARCHIVE: ArchiveCell = ArchiveCell(UnsafeCell::new(None));

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

    /// Number of entries, directories and device nodes included.
    pub fn entry_count(&self) -> usize {
        self.archive.entry_count()
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

/// Publish the archive as the root file system.
///
/// Called once from the boot path before the user program runs. A second call
/// would replace the view while descriptors still borrow the first one, so the
/// boot path installs exactly one archive.
pub fn install(initramfs: &Initramfs) {
    // SAFETY: the boot path calls this exactly once, before user mode and
    // before interrupts can run another thread. The stored archive borrows
    // module memory that the frame allocator keeps reserved for the whole boot.
    unsafe {
        *ARCHIVE.0.get() = Some(initramfs.archive);
    }
}

/// Resolve an absolute path against the installed archive.
///
/// Returns `Ok(None)` when no archive is installed, which is the boot without
/// an initramfs, and when the archive has no such entry. A path that climbs
/// above the root is reported as [`PathError::EscapesRoot`].
pub fn resolve(path: &[u8]) -> Result<Option<Entry<'static>>, PathError> {
    // SAFETY: the cell was written once before user mode and is never mutated
    // afterwards. `Archive` is `Copy`, so the read yields an independent view.
    let archive = unsafe { *ARCHIVE.0.get() };
    match archive {
        Some(archive) => archive.resolve(path),
        None => Ok(None),
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
