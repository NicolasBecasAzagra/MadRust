// SPDX-License-Identifier: GPL-2.0-only
//! Uncompressed `cpio` archives in the `newc` format.
//!
//! The benchmark rig hands the same uncompressed archive to both kernels: a
//! mainline Linux initramfs is a `newc` archive too, and the format is simple
//! enough to parse without a decompressor. The layout of one entry is a fixed
//! 110 byte header, the NUL terminated name, padding to a four byte boundary,
//! the file data and padding to the same boundary:
//!
//! ```text
//! +--------+--------+-----+--------+------+-----+
//! | header |  name  | pad |  data  | pad  | ... |
//! +--------+--------+-----+--------+------+-----+
//! ```
//!
//! Every number in the header is eight ASCII hexadecimal digits. The archive
//! ends with an entry named `TRAILER!!!`.
//!
//! Nothing here touches hardware or allocates memory: [`Archive::parse`] only
//! borrows the archive bytes and every read is bounds checked, so the same code
//! runs in the kernel and in host unit tests.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]
#![forbid(unsafe_code)]

/// Size of the fixed part of one `newc` header.
pub const HEADER_SIZE: usize = 110;

/// Magic of the `newc` format.
pub const MAGIC_NEWC: &[u8; 6] = b"070701";

/// Magic of the checksummed variant; the layout is identical.
pub const MAGIC_CRC: &[u8; 6] = b"070702";

/// Name of the last entry of every archive.
pub const TRAILER_NAME: &[u8] = b"TRAILER!!!";

/// Alignment of names and file data.
const ALIGN: usize = 4;

/// Mask that selects the file type bits of the mode field.
const S_IFMT: u32 = 0o170000;
/// Regular file.
const S_IFREG: u32 = 0o100000;
/// Any execute bit.
const EXECUTE_BITS: u32 = 0o111;

/// Reason why an archive is not usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The archive ends before the structure being read.
    TooSmall { needed: usize, found: usize },
    /// An entry does not start with `070701` or `070702`.
    BadMagic { offset: usize },
    /// A header field is not eight ASCII hexadecimal digits.
    BadNumber { offset: usize },
    /// The name length is zero or the name is not NUL terminated.
    BadName { offset: usize },
    /// The name or the file data runs past the end of the archive.
    Truncated { offset: usize },
}

/// One entry of the archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    name: &'a [u8],
    mode: u32,
    data: &'a [u8],
}

impl<'a> Entry<'a> {
    /// Path as stored in the archive, without the terminating NUL.
    pub const fn name(&self) -> &'a [u8] {
        self.name
    }

    /// Unix mode including the file type bits.
    pub const fn mode(&self) -> u32 {
        self.mode
    }

    /// File contents.
    pub const fn data(&self) -> &'a [u8] {
        self.data
    }

    /// `true` when this entry is a regular file.
    pub const fn is_regular_file(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }

    /// `true` when this entry is a regular file with an execute bit set.
    pub const fn is_executable(&self) -> bool {
        self.is_regular_file() && self.mode & EXECUTE_BITS != 0
    }

    /// Path as UTF-8, or `None` when the archive stores non-UTF-8 bytes.
    pub fn name_str(&self) -> Option<&'a str> {
        core::str::from_utf8(self.name).ok()
    }

    /// Path without a leading `./` or `/`, so `bench` matches every spelling.
    pub fn normalized_name(&self) -> Option<&'a str> {
        let name = self.name_str()?;
        let name = name.strip_prefix("./").unwrap_or(name);
        let name = name.strip_prefix('/').unwrap_or(name);
        Some(name)
    }
}

/// A validated archive that borrows the original bytes.
#[derive(Clone, Copy, Debug)]
pub struct Archive<'a> {
    bytes: &'a [u8],
}

impl<'a> Archive<'a> {
    /// Validate every entry and return a view of the archive.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ParseError> {
        let archive = Self { bytes };
        let mut offset = 0;
        loop {
            let Some((entry, next)) = archive.entry_at(offset)? else {
                return Ok(archive);
            };
            offset = next;
            if entry.name == TRAILER_NAME {
                return Ok(archive);
            }
        }
    }

    /// Bytes the archive was parsed from.
    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Iterate over the entries, in archive order.
    ///
    /// The iterator stops at the trailer or at the first malformed entry;
    /// [`Archive::parse`] has already rejected an archive with a malformed
    /// entry, so the stop only matters for defensive use.
    pub fn entries(&self) -> Entries<'a> {
        Entries {
            bytes: self.bytes,
            offset: 0,
            done: false,
        }
    }

    /// Iterate over the regular files only.
    pub fn files(&self) -> impl Iterator<Item = Entry<'a>> + 'a {
        self.entries().filter(Entry::is_regular_file)
    }

    /// Number of regular files.
    pub fn file_count(&self) -> usize {
        self.files().count()
    }

    /// Find the program a benchmark run should start.
    ///
    /// `/bench` wins when the archive carries it; otherwise the first regular
    /// file with an execute bit is the program.
    pub fn find_program(&self) -> Option<Entry<'a>> {
        let mut first_executable = None;
        for entry in self.files() {
            if entry.normalized_name() == Some("bench") {
                return Some(entry);
            }
            if first_executable.is_none() && entry.is_executable() {
                first_executable = Some(entry);
            }
        }
        first_executable
    }

    /// Parse one entry starting at `offset`.
    ///
    /// Returns `None` at the trailer, and never reads outside `bytes`.
    fn entry_at(&self, offset: usize) -> Result<Option<(Entry<'a>, usize)>, ParseError> {
        let header_end = offset
            .checked_add(HEADER_SIZE)
            .ok_or(ParseError::Truncated { offset })?;
        let header = self
            .bytes
            .get(offset..header_end)
            .ok_or(ParseError::TooSmall {
                needed: header_end,
                found: self.bytes.len(),
            })?;

        let magic = header.get(0..6).ok_or(ParseError::BadMagic { offset })?;
        if magic != MAGIC_NEWC && magic != MAGIC_CRC {
            return Err(ParseError::BadMagic { offset });
        }

        let mode = number(header, 14, offset)?;
        let file_size = number(header, 54, offset)?;
        let name_size = number(header, 94, offset)?;

        let name_start = header_end;
        let name_end = name_start
            .checked_add(name_size as usize)
            .ok_or(ParseError::BadName { offset })?;
        let name_field = self
            .bytes
            .get(name_start..name_end)
            .ok_or(ParseError::Truncated { offset })?;
        let Some((&0, name)) = name_field.split_last() else {
            return Err(ParseError::BadName { offset });
        };

        let data_start = align4(name_end);
        let data_end = data_start
            .checked_add(file_size as usize)
            .ok_or(ParseError::Truncated { offset })?;
        let data = self
            .bytes
            .get(data_start..data_end)
            .ok_or(ParseError::Truncated { offset })?;
        let next = align4(data_end);

        if next > self.bytes.len() {
            return Err(ParseError::Truncated { offset });
        }

        Ok(Some((Entry { name, mode, data }, next)))
    }
}

/// Iterator over the entries of an [`Archive`].
pub struct Entries<'a> {
    bytes: &'a [u8],
    offset: usize,
    done: bool,
}

impl<'a> Iterator for Entries<'a> {
    type Item = Entry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let archive = Archive { bytes: self.bytes };
        match archive.entry_at(self.offset) {
            Ok(Some((entry, next))) => {
                self.offset = next;
                if entry.name == TRAILER_NAME {
                    self.done = true;
                    return None;
                }
                Some(entry)
            }
            Ok(None) | Err(_) => {
                self.done = true;
                None
            }
        }
    }
}

/// Read one eight digit hexadecimal number from `header`.
///
/// `at` is the offset inside the fixed header; `entry_offset` only serves the
/// error report.
fn number(header: &[u8], at: usize, entry_offset: usize) -> Result<u32, ParseError> {
    let end = at.checked_add(8).ok_or(ParseError::BadNumber {
        offset: entry_offset,
    })?;
    let field = header.get(at..end).ok_or(ParseError::BadNumber {
        offset: entry_offset,
    })?;
    let mut value = 0u32;
    for &byte in field {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => {
                return Err(ParseError::BadNumber {
                    offset: entry_offset,
                })
            }
        };
        value = (value << 4) | u32::from(digit);
    }
    Ok(value)
}

/// Round `value` up to the archive alignment.
const fn align4(value: usize) -> usize {
    (value + ALIGN - 1) & !(ALIGN - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Append one entry to a `newc` archive being built by a test.
    fn push_entry(out: &mut Vec<u8>, name: &str, mode: u32, data: &[u8]) {
        let start = out.len();
        let header = format!(
            "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
            start, // inode
            mode,
            0, // uid
            0, // gid
            1, // nlink
            0, // mtime
            data.len(),
            0, // devmajor
            0, // devminor
            0, // rdevmajor
            0, // rdevminor
            name.len() + 1,
            0, // check
        );
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        while !out.len().is_multiple_of(ALIGN) {
            out.push(0);
        }
        out.extend_from_slice(data);
        while !out.len().is_multiple_of(ALIGN) {
            out.push(0);
        }
    }

    fn archive(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, mode, data) in entries {
            push_entry(&mut out, name, *mode, data);
        }
        push_entry(&mut out, "TRAILER!!!", 0, &[]);
        out
    }

    #[test]
    fn parses_entries_and_data() {
        let bytes = archive(&[
            (".", 0o040755, &[]),
            ("./hello", 0o100755, b"hello world"),
            ("./data", 0o100644, b"1234"),
        ]);
        let archive = Archive::parse(&bytes).expect("valid archive");
        assert_eq!(archive.file_count(), 2);
        let entries: Vec<_> = archive.entries().collect();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].name(), b"./hello");
        assert_eq!(entries[1].data(), b"hello world");
        assert!(entries[1].is_executable());
        assert!(!entries[2].is_executable());
        assert_eq!(entries[2].normalized_name(), Some("data"));
    }

    #[test]
    fn finds_bench_before_other_executables() {
        let bytes = archive(&[
            ("./init", 0o100755, b"init"),
            ("./bench", 0o100755, b"bench"),
            ("./tool", 0o100755, b"tool"),
        ]);
        let archive = Archive::parse(&bytes).expect("valid archive");
        let program = archive.find_program().expect("program");
        assert_eq!(program.normalized_name(), Some("bench"));
        assert_eq!(program.data(), b"bench");
    }

    #[test]
    fn falls_back_to_the_first_executable() {
        let bytes = archive(&[
            ("./readme", 0o100644, b"text"),
            ("./init", 0o100755, b"init"),
            ("./other", 0o100755, b"other"),
        ]);
        let archive = Archive::parse(&bytes).expect("valid archive");
        let program = archive.find_program().expect("program");
        assert_eq!(program.normalized_name(), Some("init"));
    }

    #[test]
    fn rejects_a_bad_magic() {
        let mut bytes = archive(&[("./file", 0o100644, b"x")]);
        bytes[0] = b'1';
        assert_eq!(
            Archive::parse(&bytes).err(),
            Some(ParseError::BadMagic { offset: 0 })
        );
    }

    #[test]
    fn rejects_a_truncated_header() {
        let bytes = archive(&[("./file", 0o100644, b"x")]);
        assert_eq!(
            Archive::parse(&bytes[..HEADER_SIZE - 1]).err(),
            Some(ParseError::TooSmall {
                needed: HEADER_SIZE,
                found: HEADER_SIZE - 1
            })
        );
    }

    #[test]
    fn rejects_a_truncated_name() {
        let mut bytes = archive(&[("./file", 0o100644, b"x")]);
        bytes.truncate(HEADER_SIZE + 2);
        assert_eq!(
            Archive::parse(&bytes).err(),
            Some(ParseError::Truncated { offset: 0 })
        );
    }

    #[test]
    fn rejects_a_non_hex_number() {
        let mut bytes = archive(&[("./file", 0o100644, b"x")]);
        bytes[14] = b'z';
        assert_eq!(
            Archive::parse(&bytes).err(),
            Some(ParseError::BadNumber { offset: 0 })
        );
    }

    #[test]
    fn rejects_an_empty_name() {
        let mut bytes = archive(&[("./file", 0o100644, b"x")]);
        bytes[94..102].copy_from_slice(b"00000000");
        assert_eq!(
            Archive::parse(&bytes).err(),
            Some(ParseError::BadName { offset: 0 })
        );
    }

    #[test]
    fn accepts_the_crc_magic() {
        let mut bytes = archive(&[("./file", 0o100755, b"data")]);
        bytes[0..6].copy_from_slice(MAGIC_CRC);
        let archive = Archive::parse(&bytes).expect("crc layout is identical");
        assert_eq!(archive.file_count(), 1);
    }
}
