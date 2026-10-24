// SPDX-License-Identifier: GPL-2.0-only
//! Storage for the buddy allocator metadata.
//!
//! The allocator never owns its metadata: it walks a [`BlockStore`] of one
//! byte per minimum-size block. The kernel backs the store with a static
//! buffer, while the host tests use a heap-allocated [`Vec`]. The algorithm
//! itself is generic over the trait and shared by both.

/// One byte of metadata per minimum-size block.
pub trait BlockStore {
    /// Number of bytes available for metadata.
    fn len(&self) -> usize;

    /// `true` when the store cannot describe a single block.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read the metadata byte at `index`.
    ///
    /// The allocator only reads indices below [`BlockStore::len`]. An
    /// out-of-range read returns a non-free marker instead of panicking, so a
    /// bookkeeping bug cannot fault the kernel.
    fn get(&self, index: usize) -> u8;

    /// Write the metadata byte at `index`, ignoring out-of-range writes.
    fn set(&mut self, index: usize, value: u8);
}

/// Metadata store backed by a static buffer owned by the caller.
#[derive(Debug)]
pub struct StaticStore {
    data: &'static mut [u8],
}

impl StaticStore {
    /// Wrap a static buffer.
    pub fn new(data: &'static mut [u8]) -> Self {
        Self { data }
    }
}

impl BlockStore for StaticStore {
    fn len(&self) -> usize {
        self.data.len()
    }

    fn get(&self, index: usize) -> u8 {
        self.data.get(index).copied().unwrap_or(u8::MAX)
    }

    fn set(&mut self, index: usize, value: u8) {
        if let Some(slot) = self.data.get_mut(index) {
            *slot = value;
        }
    }
}

/// Metadata store backed by a `Vec`, for host tests only.
#[cfg(test)]
#[derive(Debug)]
pub struct VecStore {
    data: Vec<u8>,
}

#[cfg(test)]
impl VecStore {
    /// Create a zeroed store of `len` bytes.
    pub fn new(len: usize) -> Self {
        Self { data: vec![0; len] }
    }
}

#[cfg(test)]
impl BlockStore for VecStore {
    fn len(&self) -> usize {
        self.data.len()
    }

    fn get(&self, index: usize) -> u8 {
        self.data.get(index).copied().unwrap_or(u8::MAX)
    }

    fn set(&mut self, index: usize, value: u8) {
        if let Some(slot) = self.data.get_mut(index) {
            *slot = value;
        }
    }
}
