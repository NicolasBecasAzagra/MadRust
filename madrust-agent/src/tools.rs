// SPDX-License-Identifier: GPL-2.0-only
//! The tool registry.
//!
//! Six tools, exactly the names in [`NAMES`], each with the argument keys the
//! system prompt explains. [`REQUIREMENTS`] states the keys a call has to
//! carry, so the grammar of the call refuses a tool call that cannot work
//! before the loop spends a step on it. Every tool returns one line of text:
//! the loop puts it in the history as the user turn and prints it in the
//! `AGENT` line. A refused system call is a result too, `error errno=N`, so the
//! model sees the failure and can choose another call instead of the loop
//! going silent.
//!
//! `read_file`, `write_file` and `net_send` use the standard library, which
//! reaches the kernel through the Linux calls both kernels implement.
//! `list_dir` uses the raw `openat` and `getdents64` calls the benchmark
//! workload uses, because MadRust answers `getdents64` with `ENOSYS`: the same
//! bytes then run on both kernels and the MadRust run reports the error it
//! really got.
//!
//! `net_send` sends one datagram of UDP to a server and waits for its answer.
//! The argument keys are the ones the grammar already accepts: `query` carries
//! the endpoint as `ADDRESS:PORT`, because the key list has no `address` and no
//! `port`, and `text` carries the datagram. The tool retries the receive,
//! because MadRust answers a socket with nothing to read at once with the error
//! of a would block, and the server of the host can be slower than the guest.
//! The number of retries and the round trip are in the answer.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::call::Call;
use crate::expression;

use madrust_infer::grammar::{Argument, ValueType};

/// The tool names, in the order the system prompt lists them.
pub const NAMES: [&str; 6] = [
    "read_file",
    "list_dir",
    "write_file",
    "compute",
    "net_send",
    "done",
];

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
    (
        "net_send",
        &[
            Argument::required("query", ValueType::String),
            Argument::required("text", ValueType::String),
        ],
    ),
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

/// The round trip of one datagram, as the network tool measured it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoundTrip {
    /// Microseconds between the send and the answer.
    pub micros: u64,
    /// Receive attempts that found nothing before the answer arrived.
    pub retries: u32,
}

/// One call and what it produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The text for the history, or the final answer.
    pub outcome: Outcome,
    /// The sample of a network call, when the call was `net_send` and it got
    /// an answer. The loop reports it in the summary line, so the rig can
    /// publish the round trip of the run beside the cost of the run.
    pub round_trip: Option<RoundTrip>,
}

/// Run one parsed call.
///
/// A grammar bug can still produce an unknown name or a missing argument, so
/// those become an error text too. Only `done` ends the loop.
pub fn run(call: &Call) -> Step {
    let mut round_trip = None;
    let outcome = match call.tool.as_str() {
        "read_file" => Outcome::Continue(read_file(call)),
        "list_dir" => Outcome::Continue(list_dir(call)),
        "write_file" => Outcome::Continue(write_file(call)),
        "compute" => Outcome::Continue(compute(call)),
        "net_send" => {
            let (text, sample) = net_send(call);
            round_trip = sample;
            Outcome::Continue(text)
        }
        "done" => match string_arg(call, "text") {
            Ok(text) => Outcome::Done(text.to_string()),
            Err(error) => Outcome::Continue(error),
        },
        other => Outcome::Continue(format!("error unknown tool {other}")),
    };
    Step {
        outcome,
        round_trip,
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
    let read = match file.take(MAX_CONTENT).read_to_end(&mut content) {
        Ok(read) => read,
        Err(error) => return io_error(&error),
    };
    let bytes = size.unwrap_or(read as u64);
    // The result of a tool goes back into the prompt, and the prompt is
    // encoded and prefilled again on every step. A file of four kilobytes
    // would make the next step cost four kilobytes more prefill, and the
    // agent would spend its budget reading the same file again. The answer
    // carries the size of the file and a short preview of it.
    let preview = String::from_utf8_lossy(&content);
    let preview: String = preview.chars().take(PREVIEW_CHARS).collect();
    format!("bytes={bytes} read={read} content={preview}")
}

/// Bytes of a file the `read_file` tool reads.
const MAX_CONTENT: u64 = 4096;

/// Characters of the file the `read_file` tool puts in its answer.
const PREVIEW_CHARS: usize = 256;

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

/// Send one datagram and wait for the answer of the server.
///
/// The endpoint comes in one argument, `ADDRESS:PORT`, because the grammar of a
/// call does not carry a key for an address and one for a port. The receive is
/// retried: MadRust answers a socket with nothing to read at once with the error
/// of a would block, and the server of the host may answer after the guest has
/// asked. The answer is cut to [`ANSWER_CHARS`] characters, because it goes back
/// into the prompt of the next step, and the text says how much was left out.
fn net_send(call: &Call) -> (String, Option<RoundTrip>) {
    let query = match string_arg(call, "query") {
        Ok(query) => query,
        Err(error) => return (error, None),
    };
    let text = match string_arg(call, "text") {
        Ok(text) => text,
        Err(error) => return (error, None),
    };
    let endpoint = match parse_endpoint(query) {
        Ok(endpoint) => endpoint,
        Err(error) => return (error, None),
    };

    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        Ok(socket) => socket,
        Err(error) => return (io_error(&error), None),
    };
    // A kernel that does not implement the timeout of a socket refuses the
    // option. The loop below then relies on the non-blocking read alone, which
    // is what MadRust does.
    let _ = socket.set_read_timeout(Some(READ_TIMEOUT));
    if let Err(error) = socket.connect(endpoint) {
        return (io_error(&error), None);
    }

    let started = Instant::now();
    if let Err(error) = socket.send(text.as_bytes()) {
        return (io_error(&error), None);
    }

    let mut buffer = [0u8; ANSWER_BYTES];
    let mut retries = 0u32;
    loop {
        match socket.recv(&mut buffer) {
            Ok(length) => {
                let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                let answer = String::from_utf8_lossy(&buffer[..length]);
                let total = answer.chars().count();
                let preview: String = answer.chars().take(ANSWER_CHARS).collect();
                let mut result =
                    format!("answer={preview} bytes={length} retries={retries} rt_us={micros}");
                if total > ANSWER_CHARS {
                    // The prompt of the next step carries this text, so the
                    // tool says what it did not carry instead of silently
                    // cutting the answer of the server.
                    let _ = write!(result, " omitted={}", total - ANSWER_CHARS);
                }
                return (result, Some(RoundTrip { micros, retries }));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                retries = retries.saturating_add(1);
                if started.elapsed() >= ANSWER_TIMEOUT {
                    return (format!("error no answer retries={retries}"), None);
                }
            }
            Err(error) => return (io_error(&error), None),
        }
    }
}

/// Parse the `ADDRESS:PORT` argument of the network tool.
fn parse_endpoint(query: &str) -> Result<SocketAddr, String> {
    let Some((address, port)) = query.rsplit_once(':') else {
        return Err(format!("error endpoint is not ADDRESS:PORT: {query}"));
    };
    let address: Ipv4Addr = address
        .parse()
        .map_err(|_| format!("error endpoint is not an IPv4 address: {query}"))?;
    let port: u16 = port
        .parse()
        .map_err(|_| format!("error endpoint is not a port: {query}"))?;
    if port == 0 {
        return Err(format!("error endpoint port is zero: {query}"));
    }
    Ok(SocketAddr::from((address, port)))
}

/// Largest answer the network tool reads, in bytes.
const ANSWER_BYTES: usize = 1024;

/// Characters of the answer the network tool puts in its result.
const ANSWER_CHARS: usize = 256;

/// How long the network tool waits for an answer, in total.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(1);

/// Wait of one receive attempt on a kernel that honours the socket timeout.
const READ_TIMEOUT: Duration = Duration::from_millis(50);

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

    /// The outcome of one call, without the network sample.
    fn outcome(tool: &str, args: &[(&str, Value)]) -> Outcome {
        run(&call(tool, args)).outcome
    }

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

    /// `true` when `bytes` is a complete call of `grammar`.
    fn accepts(grammar: &Grammar, bytes: &[u8]) -> bool {
        let mut state = grammar.start();
        for &byte in bytes {
            match grammar.advance(state, &[byte]) {
                Some(next) => state = next,
                None => return false,
            }
        }
        grammar.is_complete(state)
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
            .expect("every declared key is an accepted argument");
        for call in [
            br#"{"tool":"read_file","args":{"path":"/a"}}"#.as_slice(),
            br#"{"tool":"list_dir","args":{"path":"/a"}}"#,
            br#"{"tool":"write_file","args":{"path":"/a","text":"b"}}"#,
            br#"{"tool":"compute","args":{"value":"1+1"}}"#,
            br#"{"tool":"net_send","args":{"query":"10.0.2.2:9999","text":"ping"}}"#,
            br#"{"tool":"done","args":{"text":"ok"}}"#,
        ] {
            assert!(accepts(&grammar, call), "the call is not complete");
        }
        // The call the table exists to refuse: an empty object for a tool that
        // needs a path.
        assert!(!accepts(&grammar, br#"{"tool":"read_file","args":{}}"#));
    }

    #[test]
    fn a_call_carries_only_the_keys_of_its_tool() {
        let grammar = Grammar::tool_call_with_requirements(&NAMES, REQUIREMENTS)
            .expect("every declared key is an accepted argument");
        // `text` belongs to `write_file`. The run that started this rule wrote
        // `{"tool":"read_file","args":{"path":"/vocab.json","text":...}}`.
        assert!(!accepts(
            &grammar,
            br#"{"tool":"read_file","args":{"path":"/a","text":"b"}}"#
        ));
        // A key cannot come twice.
        assert!(!accepts(
            &grammar,
            br#"{"tool":"read_file","args":{"path":"/a","path":"/b"}}"#
        ));
    }

    #[test]
    fn the_registry_lists_the_six_tools() {
        assert_eq!(
            NAMES,
            [
                "read_file",
                "list_dir",
                "write_file",
                "compute",
                "net_send",
                "done"
            ]
        );
    }

    #[test]
    fn writes_reads_and_lists_one_file() {
        let directory = temp_dir("io");
        let path = directory.join("a.txt");
        let path_text = path.to_string_lossy().into_owned();

        let written = outcome(
            "write_file",
            &[("path", text(&path_text)), ("text", text("hello"))],
        );
        assert_eq!(written, Outcome::Continue("bytes=5".to_string()));

        let read = outcome("read_file", &[("path", text(&path_text))]);
        let Outcome::Continue(read) = read else {
            panic!("read_file must not end the loop");
        };
        assert!(read.starts_with("bytes=5 read=5 content=hello"), "{read}");

        let listed = outcome(
            "list_dir",
            &[("path", text(directory.to_string_lossy().as_ref()))],
        );
        let Outcome::Continue(listed) = listed else {
            panic!("list_dir must not end the loop");
        };
        assert!(listed.starts_with("entries=1 "), "{listed}");
        assert!(listed.contains("a.txt"), "{listed}");

        std::fs::remove_dir_all(&directory).expect("the temporary directory is removed");
    }

    #[test]
    fn reports_a_refused_read_with_the_error_number() {
        let Outcome::Continue(result) = outcome(
            "read_file",
            &[("path", text("/madrust-agent/does/not/exist"))],
        ) else {
            panic!("read_file must not end the loop");
        };
        assert!(result.starts_with("error errno="), "{result}");
    }

    #[test]
    fn computes_or_reports_the_arithmetic_error() {
        assert_eq!(
            outcome("compute", &[("value", text("(2 + 3) * 4"))]),
            Outcome::Continue("value=20".to_string())
        );
        assert_eq!(
            outcome("compute", &[("value", text("1 / 0"))]),
            Outcome::Continue("error=division by zero".to_string())
        );
        assert_eq!(
            outcome("compute", &[("value", text("1 +"))]),
            Outcome::Continue("error=unexpected end of expression".to_string())
        );
    }

    #[test]
    fn done_carries_the_final_answer_and_ends_the_loop() {
        assert_eq!(
            outcome("done", &[("text", text("12 bytes"))]),
            Outcome::Done("12 bytes".to_string())
        );
        assert!(matches!(
            outcome("done", &[]),
            Outcome::Continue(error) if error.starts_with("error missing string argument")
        ));
    }

    #[test]
    fn an_unknown_tool_is_an_error_text() {
        assert_eq!(
            outcome("other", &[]),
            Outcome::Continue("error unknown tool other".to_string())
        );
    }

    /// A server on the host that answers the first datagram it receives.
    ///
    /// Without an answer of its own it echoes the bytes of the request, the way
    /// the server of the rig does.
    fn echo_once(reply: Option<Vec<u8>>) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        let server =
            std::net::UdpSocket::bind(("127.0.0.1", 0)).expect("the test server binds a free port");
        server
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("the test server waits for its datagram");
        let address = server.local_addr().expect("the test server has an address");
        let worker = std::thread::spawn(move || {
            let mut buffer = [0u8; 2048];
            let (length, peer) = server.recv_from(&mut buffer).expect("the datagram arrives");
            let answer = reply.unwrap_or_else(|| buffer[..length].to_vec());
            server.send_to(&answer, peer).expect("the answer leaves");
        });
        (address, worker)
    }

    #[test]
    fn sends_a_datagram_and_reports_the_answer_and_the_round_trip() {
        let (address, worker) = echo_once(None);
        let step = run(&call(
            "net_send",
            &[
                ("query", text(&address.to_string())),
                ("text", text("ping")),
            ],
        ));
        worker.join().expect("the test server finishes");
        let Outcome::Continue(result) = step.outcome else {
            panic!("net_send must not end the loop");
        };
        assert!(
            result.starts_with("answer=ping bytes=4 retries="),
            "{result}"
        );
        assert!(result.contains(" rt_us="), "{result}");
        assert!(
            step.round_trip.is_some(),
            "the tool measured its datagram: {result}"
        );
    }

    #[test]
    fn an_answer_longer_than_the_window_says_how_much_was_left_out() {
        let long = vec![b'a'; 300];
        let (address, worker) = echo_once(Some(long));
        let step = run(&call(
            "net_send",
            &[
                ("query", text(&address.to_string())),
                ("text", text("ping")),
            ],
        ));
        worker.join().expect("the test server finishes");
        let Outcome::Continue(result) = step.outcome else {
            panic!("net_send must not end the loop");
        };
        // 256 characters stay and the other 44 are named, not dropped in
        // silence: the text goes back into the prompt of the next step.
        assert!(result.contains("omitted=44"), "{result}");
    }

    #[test]
    fn a_bad_endpoint_is_a_reason_and_not_a_default() {
        let step = run(&call(
            "net_send",
            &[("query", text("10.0.2.2")), ("text", text("ping"))],
        ));
        assert!(step.round_trip.is_none());
        assert!(matches!(
            step.outcome,
            Outcome::Continue(error) if error.starts_with("error endpoint is not ADDRESS:PORT")
        ));
    }
}
