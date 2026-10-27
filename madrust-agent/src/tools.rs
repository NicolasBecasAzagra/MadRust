// SPDX-License-Identifier: GPL-2.0-only
//! The tool registry.
//!
//! Five tools, exactly the names in [`NAMES`], each with the argument keys the
//! system prompt explains. [`REQUIREMENTS`] states the keys a call has to
//! carry, so the grammar of the call refuses a tool call that cannot work
//! before the loop spends a step on it. Every tool returns one line of text:
//! the loop puts it in the history as the user turn and prints it in the
//! `AGENT` line. A refused system call is a result too, `error errno=N`, so the
//! model sees the failure and can choose another call instead of the loop
//! going silent.
//!
//! `read_file` and `write_file` use the standard library, which reaches the
//! kernel through the Linux calls both kernels implement. `list_dir` uses the
//! raw `openat` and `getdents64` calls the benchmark workload uses, because
//! MadRust answers `getdents64` with `ENOSYS`: the same bytes then run on both
//! kernels and the MadRust run reports the error it really got.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};

use crate::call::Call;
use crate::expression;

use madrust_infer::grammar::{Argument, ValueType};

/// The tool names, in the order the system prompt lists them.
pub const NAMES: [&str; 5] = ["read_file", "list_dir", "write_file", "compute", "done"];

/// The arguments each tool takes, by tool name.
///
/// The grammar of the call accepts a tool call only when its object carries
/// every required key, and only when the value under a key has the declared
/// type. A key here must be one of `madrust_infer::grammar::ARGUMENT_NAMES`;
/// the test of this module checks that, because a key outside that list makes
/// the grammar construction fail at start up.
pub const REQUIREMENTS: &[(&str, &[Argument<'static>])] = &[
    (
        "read_file",
        &[Argument::required("path", ValueType::String)],
    ),
    ("list_dir", &[Argument::required("path", ValueType::String)]),
    (
        "write_file",
        &[
            Argument::required("path", ValueType::String),
            Argument::required("text", ValueType::String),
        ],
    ),
    ("compute", &[Argument::required("value", ValueType::String)]),
    ("done", &[Argument::required("text", ValueType::String)]),
];

/// What one tool call produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The call did not end the run; the text goes into the history.
    Continue(String),
    /// The `done` tool's final answer.
    Done(String),
}

/// Run one parsed call.
///
/// A grammar bug can still produce an unknown name or a missing argument, so
/// those become an error text too. Only `done` ends the loop.
pub fn run(call: &Call) -> Outcome {
    match call.tool.as_str() {
        "read_file" => Outcome::Continue(read_file(call)),
        "list_dir" => Outcome::Continue(list_dir(call)),
        "write_file" => Outcome::Continue(write_file(call)),
        "compute" => Outcome::Continue(compute(call)),
        "done" => match string_arg(call, "text") {
            Ok(text) => Outcome::Done(text.to_string()),
            Err(error) => Outcome::Continue(error),
        },
        other => Outcome::Continue(format!("error unknown tool {other}")),
    }
}

/// Open a path read only, read up to 4096 bytes and report the size.
///
/// The size is the file's length from its metadata, so a file larger than the
/// read window still answers "how many bytes it has". `read` says how many
/// bytes the window actually carried.
fn read_file(call: &Call) -> String {
    let path = match string_arg(call, "path") {
        Ok(path) => path,
        Err(error) => return error,
    };
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => return io_error(&error),
    };
    let size = file.metadata().map(|metadata| metadata.len()).ok();
    let mut content = Vec::new();
    let read = match file.take(4096).read_to_end(&mut content) {
        Ok(read) => read,
        Err(error) => return io_error(&error),
    };
    let bytes = size.unwrap_or(read as u64);
    format!(
        "bytes={bytes} read={read} content={}",
        String::from_utf8_lossy(&content)
    )
}

/// Create or truncate a file and write the text.
fn write_file(call: &Call) -> String {
    let path = match string_arg(call, "path") {
        Ok(path) => path,
        Err(error) => return error,
    };
    let text = match string_arg(call, "text") {
        Ok(text) => text,
        Err(error) => return error,
    };
    let mut file = match OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) => return io_error(&error),
    };
    match file.write_all(text.as_bytes()) {
        Ok(()) => format!("bytes={}", text.len()),
        Err(error) => io_error(&error),
    }
}

/// Evaluate an integer expression.
fn compute(call: &Call) -> String {
    let value = match string_arg(call, "value") {
        Ok(value) => value,
        Err(error) => return error,
    };
    match expression::evaluate(value) {
        Ok(value) => format!("value={value}"),
        Err(error) => format!("error={error}"),
    }
}

/// List the entries of a directory.
fn list_dir(call: &Call) -> String {
    let path = match string_arg(call, "path") {
        Ok(path) => path,
        Err(error) => return error,
    };
    match list_entries(path) {
        Ok(entries) => format!("entries={} {}", entries.len(), entries.join(", ")),
        Err(errno) => format!("error errno={errno}"),
    }
}

/// Read one required string argument.
fn string_arg<'a>(call: &'a Call, key: &str) -> Result<&'a str, String> {
    match call.string(key) {
        Some(value) => Ok(value),
        None => Err(format!("error missing string argument {key}")),
    }
}

/// Report a standard library I/O failure by its error number.
fn io_error(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(number) => format!("error errno={number}"),
        None => format!("error {error}"),
    }
}

/// List a directory through the raw Linux calls.
///
/// `getdents64` fills a buffer with `struct linux_dirent64` records: an 8 byte
/// inode, an 8 byte offset, a 2 byte record length at byte 16, a type byte and
/// then the NUL terminated name at byte 19. The path is opened with
/// `O_DIRECTORY`, so a path that is not a directory fails at the open.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn list_entries(path: &str) -> Result<Vec<String>, i64> {
    const AT_FDCWD: u64 = -100i64 as u64;
    const O_RDONLY: u64 = 0;
    const O_DIRECTORY: u64 = 0x1_0000;
    const SYS_CLOSE: u64 = 3;
    const SYS_GETDENTS64: u64 = 217;
    const SYS_OPENAT: u64 = 257;
    const EIO: i64 = 5;
    const EINVAL: i64 = 22;

    if path.as_bytes().contains(&0) {
        return Err(EINVAL);
    }
    let mut name = Vec::with_capacity(path.len() + 1);
    name.extend_from_slice(path.as_bytes());
    name.push(0);

    // SAFETY: openat takes four arguments, `name` is a live NUL terminated
    // string, and both kernels implement the call.
    let descriptor = unsafe {
        syscall4(
            SYS_OPENAT,
            AT_FDCWD,
            name.as_ptr() as u64,
            O_RDONLY | O_DIRECTORY,
            0,
        )
    };
    if descriptor < 0 {
        return Err(error_number(descriptor));
    }
    let descriptor = descriptor as u64;

    let mut entries = Vec::new();
    let mut buffer = [0u8; 4096];
    let result = 'read: loop {
        // SAFETY: the descriptor is open and the buffer is live for the call.
        let count = unsafe {
            syscall3(
                SYS_GETDENTS64,
                descriptor,
                buffer.as_mut_ptr() as u64,
                buffer.len() as u64,
            )
        };
        if count < 0 {
            break 'read Err(error_number(count));
        }
        if count == 0 {
            break 'read Ok(());
        }
        let count = count as usize;
        let mut offset = 0usize;
        while offset < count {
            if offset + 19 > count {
                break 'read Err(EIO);
            }
            let record = u16::from_le_bytes([buffer[offset + 16], buffer[offset + 17]]) as usize;
            if record < 20 || offset + record > count {
                break 'read Err(EIO);
            }
            let name_bytes = &buffer[offset + 19..offset + record];
            let end = name_bytes
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(name_bytes.len());
            let name = String::from_utf8_lossy(&name_bytes[..end]).into_owned();
            if name != "." && name != ".." {
                entries.push(name);
            }
            offset += record;
        }
    };

    // SAFETY: the descriptor came from open and close takes one argument.
    let _ = unsafe { syscall1(SYS_CLOSE, descriptor) };
    result?;
    entries.sort();
    entries.dedup();
    Ok(entries)
}

/// Fallback for a host that is not Linux x86-64, where the host tests run.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn list_entries(path: &str) -> Result<Vec<String>, i64> {
    let directory = std::fs::read_dir(path).map_err(io_errno)?;
    let mut entries = Vec::new();
    for entry in directory {
        let entry = entry.map_err(io_errno)?;
        entries.push(entry.file_name().to_string_lossy().into_owned());
    }
    entries.sort();
    entries.dedup();
    Ok(entries)
}

/// Turn a standard library I/O error into a positive Linux error number.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn io_errno(error: std::io::Error) -> i64 {
    error.raw_os_error().map(i64::from).unwrap_or(5)
}

/// The positive number of a negative system call result.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn error_number(result: i64) -> i64 {
    result.saturating_neg()
}

/// Raw `syscall` with one argument.
///
/// # Safety
///
/// The caller must pass a system call and arguments valid for the running
/// kernel. The assembly only issues the instruction.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn syscall1(number: u64, argument1: u64) -> i64 {
    let result: i64;
    // SAFETY: the caller upholds the contract above.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") argument1,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Raw `syscall` with three arguments.
///
/// # Safety
///
/// See [`syscall1`].
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn syscall3(number: u64, argument1: u64, argument2: u64, argument3: u64) -> i64 {
    let result: i64;
    // SAFETY: the caller upholds the contract above.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") argument1,
            in("rsi") argument2,
            in("rdx") argument3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Raw `syscall` with four arguments.
///
/// # Safety
///
/// See [`syscall1`].
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn syscall4(
    number: u64,
    argument1: u64,
    argument2: u64,
    argument3: u64,
    argument4: u64,
) -> i64 {
    let result: i64;
    // SAFETY: the caller upholds the contract above.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") argument1,
            in("rsi") argument2,
            in("rdx") argument3,
            in("r10") argument4,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    use madrust_infer::grammar::{Grammar, ARGUMENT_NAMES};

    use super::{run, Outcome, NAMES, REQUIREMENTS};
    use crate::call::{Call, Value};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "madrust-agent-{}-{}-{label}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("the temporary directory is created");
        path
    }

    fn call(tool: &str, args: &[(&str, Value)]) -> Call {
        Call {
            tool: tool.to_string(),
            args: args
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn text(value: &str) -> Value {
        Value::String(value.to_string())
    }

    #[test]
    fn every_required_key_is_an_accepted_argument() {
        for (tool, arguments) in REQUIREMENTS {
            assert!(
                NAMES.contains(tool),
                "the requirements name the unknown tool {tool}"
            );
            for argument in *arguments {
                assert!(
                    ARGUMENT_NAMES.contains(&argument.key),
                    "{tool} declares {}, which the grammar does not accept",
                    argument.key
                );
            }
        }
        // A tool with no entry would accept an empty object, the call this
        // table exists to refuse.
        for name in NAMES {
            assert!(
                REQUIREMENTS.iter().any(|(tool, _)| *tool == name),
                "{name} states no requirements"
            );
        }
    }

    #[test]
    fn the_path_of_read_file_is_a_string() {
        let grammar = Grammar::tool_call_with_requirements(&NAMES, REQUIREMENTS)
            .expect("every declared key is an accepted argument");
        let after_colon = grammar
            .advance(grammar.start(), br#"{"tool":"read_file","args":{"path":"#)
            .expect("the key and its colon are legal");
        assert_eq!(
            grammar
                .allowed_bytes(after_colon)
                .iter()
                .collect::<Vec<_>>(),
            vec![usize::from(b'"')],
            "a boolean must not start the value of `path`"
        );
    }

    #[test]
    fn the_grammar_accepts_the_keys_of_every_tool() {
        let grammar = Grammar::tool_call_with_requirements(&NAMES, REQUIREMENTS)
            .expect("every required key is an accepted argument");
        for call in [
            br#"{"tool":"read_file","args":{"path":"/a"}}"#.as_slice(),
            br#"{"tool":"list_dir","args":{"path":"/a"}}"#,
            br#"{"tool":"write_file","args":{"path":"/a","text":"b"}}"#,
            br#"{"tool":"compute","args":{"value":"1+1"}}"#,
            br#"{"tool":"done","args":{"text":"ok"}}"#,
        ] {
            let mut state = grammar.start();
            for &byte in call {
                let next = grammar.advance(state, &[byte]);
                assert!(next.is_some(), "the call left the grammar");
                state = next.expect("the byte has a transition");
            }
            assert!(grammar.is_complete(state), "the call is not complete");
        }
        // The call the table exists to refuse: an empty object for a tool that
        // needs a path. The closing brace leaves the language, or the call
        // reaches its end without being complete.
        let mut state = grammar.start();
        for &byte in br#"{"tool":"read_file","args":{}}"# {
            match grammar.advance(state, &[byte]) {
                Some(next) => state = next,
                None => break,
            }
        }
        assert!(!grammar.is_complete(state));
    }

    #[test]
    fn the_registry_lists_the_five_tools() {
        assert_eq!(
            NAMES,
            ["read_file", "list_dir", "write_file", "compute", "done"]
        );
    }

    #[test]
    fn writes_reads_and_lists_one_file() {
        let directory = temp_dir("io");
        let path = directory.join("a.txt");
        let path_text = path.to_string_lossy().into_owned();

        let written = run(&call(
            "write_file",
            &[("path", text(&path_text)), ("text", text("hello"))],
        ));
        assert_eq!(written, Outcome::Continue("bytes=5".to_string()));

        let read = run(&call("read_file", &[("path", text(&path_text))]));
        let Outcome::Continue(read) = read else {
            panic!("read_file must not end the loop");
        };
        assert!(read.starts_with("bytes=5 read=5 content=hello"), "{read}");

        let listed = run(&call(
            "list_dir",
            &[("path", text(directory.to_string_lossy().as_ref()))],
        ));
        let Outcome::Continue(listed) = listed else {
            panic!("list_dir must not end the loop");
        };
        assert!(listed.starts_with("entries=1 "), "{listed}");
        assert!(listed.contains("a.txt"), "{listed}");

        std::fs::remove_dir_all(&directory).expect("the temporary directory is removed");
    }

    #[test]
    fn reports_a_refused_read_with_the_error_number() {
        let Outcome::Continue(result) = run(&call(
            "read_file",
            &[("path", text("/madrust-agent/does/not/exist"))],
        )) else {
            panic!("read_file must not end the loop");
        };
        assert!(result.starts_with("error errno="), "{result}");
    }

    #[test]
    fn computes_or_reports_the_arithmetic_error() {
        assert_eq!(
            run(&call("compute", &[("value", text("(2 + 3) * 4"))])),
            Outcome::Continue("value=20".to_string())
        );
        assert_eq!(
            run(&call("compute", &[("value", text("1 / 0"))])),
            Outcome::Continue("error=division by zero".to_string())
        );
        assert_eq!(
            run(&call("compute", &[("value", text("1 +"))])),
            Outcome::Continue("error=unexpected end of expression".to_string())
        );
    }

    #[test]
    fn done_carries_the_final_answer_and_ends_the_loop() {
        assert_eq!(
            run(&call("done", &[("text", text("12 bytes"))])),
            Outcome::Done("12 bytes".to_string())
        );
        assert!(matches!(
            run(&call("done", &[])),
            Outcome::Continue(error) if error.starts_with("error missing string argument")
        ));
    }

    #[test]
    fn an_unknown_tool_is_an_error_text() {
        assert_eq!(
            run(&call("other", &[])),
            Outcome::Continue("error unknown tool other".to_string())
        );
    }
}
