// SPDX-License-Identifier: GPL-2.0-only
//! The tool registry.
//!
//! Seven tools, exactly the names in [`NAMES`], each with the argument keys the
//! system prompt explains. [`REQUIREMENTS`] states the keys a call has to
//! carry, so the grammar of the call refuses a tool call that cannot work
//! before the loop spends a step on it. Every tool returns one line of text:
//! the loop puts it in the history as the user turn and prints it in the
//! `AGENT` line. A refused system call is a result too, `error errno=N`, so the
//! model sees the failure and can choose another call instead of the loop
//! going silent.
//!
//! `read_file`, `write_file`, `net_send` and `http_get` use the standard library, which
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
//!
//! The tool configures the interface of the guest before its first datagram,
//! because MadRust brings the card up with the address of the emulator and a
//! mainline kernel leaves it down and unnumbered: the send of the same program
//! then fails with `ENETUNREACH` and the run measures nothing. [`crate::link`]
//! makes the calls and reports what the kernel answered, and the result of the
//! tool carries that report as `link=`, so a kernel that refuses the setup says
//! so on the line of the tool.
//!
//! `http_get` makes the request of a client of an API: one `GET` over a stream
//! socket, the `Host` header included, through [`madrust_http`]. The argument is
//! a whole address in the form a person writes it, `http://HOST:PORT/PATH`, and
//! the result carries the status, the headers that matter and a bounded body
//! with the number of characters the bound left out. The read is retried for
//! the same reason as the datagram: a kernel with nothing to read answers a
//! would block, and the server of the host can be slower than the guest.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use madrust_http::{write_request, MessageReader, Method, Progress, Url};

use crate::call::Call;
use crate::expression;

use madrust_infer::grammar::{Argument, ValueType};

/// The tool names, in the order the system prompt lists them.
pub const NAMES: [&str; 7] = [
    "read_file",
    "list_dir",
    "write_file",
    "compute",
    "net_send",
    "http_get",
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
    (
        "http_get",
        &[Argument::required("query", ValueType::String)],
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
    /// The sample of the datagram, when the call was `net_send` and it got an
    /// answer. The loop reports it in the summary line, so the rig can publish
    /// the round trip of the run beside the cost of the run.
    pub net: Option<RoundTrip>,
    /// The sample of the HTTP call, when the call was `http_get` and it got an
    /// answer. It has a key of its own on the summary line, because a datagram
    /// and a request over a stream are two different measurements.
    pub http: Option<RoundTrip>,
}

/// The samples one run took, one per kind of call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Samples {
    /// Round trip of the datagram, when the run sent one.
    pub net: Option<RoundTrip>,
    /// Round trip of the HTTP call, when the run made one.
    pub http: Option<RoundTrip>,
}

impl Samples {
    /// Keep the sample a call produced, if it produced one.
    pub fn absorb(&mut self, step: &Step) {
        if step.net.is_some() {
            self.net = step.net;
        }
        if step.http.is_some() {
            self.http = step.http;
        }
    }
}

/// Run one parsed call.
///
/// A grammar bug can still produce an unknown name or a missing argument, so
/// those become an error text too. Only `done` ends the loop.
pub fn run(call: &Call) -> Step {
    let mut net = None;
    let mut http = None;
    let outcome = match call.tool.as_str() {
        "read_file" => Outcome::Continue(read_file(call)),
        "list_dir" => Outcome::Continue(list_dir(call)),
        "write_file" => Outcome::Continue(write_file(call)),
        "compute" => Outcome::Continue(compute(call)),
        "net_send" => {
            let (text, sample) = net_send(call);
            net = sample;
            Outcome::Continue(text)
        }
        "http_get" => {
            let (text, sample) = http_get(call);
            http = sample;
            Outcome::Continue(text)
        }
        "done" => match string_arg(call, "text") {
            Ok(text) => Outcome::Done(text.to_string()),
            Err(error) => Outcome::Continue(error),
        },
        other => Outcome::Continue(format!("error unknown tool {other}")),
    };
    Step { outcome, net, http }
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
///
/// The interface is configured before the socket is opened, because the send
/// needs a route: MadRust has one from its own setup and a mainline kernel has
/// none until a program of the guest makes it. The state of the link is in the
/// result of every path, so a refused call of the interface is in the log beside
/// the error of the send it caused.
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

    let link = crate::link::ensure();
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        Ok(socket) => socket,
        Err(error) => return (with_link(io_error(&error), link), None),
    };
    // A kernel that does not implement the timeout of a socket refuses the
    // option. The loop below then relies on the non-blocking read alone, which
    // is what MadRust does.
    let _ = socket.set_read_timeout(Some(READ_TIMEOUT));
    if let Err(error) = socket.connect(endpoint) {
        return (with_link(io_error(&error), link), None);
    }

    let started = Instant::now();
    if let Err(error) = socket.send(text.as_bytes()) {
        return (with_link(io_error(&error), link), None);
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
                return (with_link(result, link), Some(RoundTrip { micros, retries }));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                retries = retries.saturating_add(1);
                if started.elapsed() >= ANSWER_TIMEOUT {
                    return (
                        with_link(format!("error no answer retries={retries}"), link),
                        None,
                    );
                }
            }
            Err(error) => return (with_link(io_error(&error), link), None),
        }
    }
}

/// Put the state of the link on the result of the network tool.
///
/// The state is on every path and not only on a failure: a kernel that refuses
/// the setup and still sends, and a kernel that accepts it, then sit in the log
/// next to each other with the round trip each measured.
fn with_link(mut result: String, link: &crate::link::Link) -> String {
    let _ = write!(result, " link={}", link.report());
    result
}

/// Make one HTTP request and report the answer of the server.
///
/// The argument is the address in the form a person writes it,
/// `http://HOST:PORT/PATH`, and [`Url`] takes it apart. The request line and
/// the `Host` header come from [`write_request`], so a request of this tool
/// always names the server it connects to. The answer is read through
/// [`MessageReader`], which joins the pieces a socket hands over and decodes a
/// chunked body, and the read is retried because MadRust answers a socket with
/// nothing to read at once with the error of a would block.
///
/// The body goes back into the prompt of the next step, so it is cut to
/// [`ANSWER_CHARS`] characters and the result says how many were left out. A
/// body the server did not send and a body of zero bytes are different results:
/// `body=none` and `bytes=0`.
fn http_get(call: &Call) -> (String, Option<RoundTrip>) {
    let query = match string_arg(call, "query") {
        Ok(query) => query,
        Err(error) => return (error, None),
    };
    let url = match Url::parse(query) {
        Ok(url) => url,
        Err(error) => return (format!("error {error}"), None),
    };
    let host = url
        .host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(url.host);
    let authority = format!("{}:{}", url.host, url.port);
    let (path, query_part) = url.target_parts();
    let target = format!("{path}{query_part}");

    let mut request = [0u8; REQUEST_BYTES];
    let written = match write_request(
        &mut request,
        Method::Get,
        &target,
        &authority,
        &[
            ("accept", "*/*"),
            ("user-agent", "madrust-agent/0.1"),
            ("connection", "close"),
        ],
        None,
    ) {
        Ok(written) => written,
        Err(error) => return (format!("error {error}"), None),
    };

    let link = crate::link::ensure();
    let mut socket = match TcpStream::connect((host, url.port)) {
        Ok(socket) => socket,
        Err(error) => return (with_link(io_error(&error), link), None),
    };
    // A kernel that does not implement the timeout of a socket refuses the
    // option; the loop below then relies on the retries alone.
    let _ = socket.set_read_timeout(Some(READ_TIMEOUT));
    if let Err(error) = socket.write_all(&request[..written]) {
        return (with_link(io_error(&error), link), None);
    }

    let started = Instant::now();
    let mut input = [0u8; RESPONSE_BYTES];
    let mut body = [0u8; BODY_BYTES];
    let mut reader = MessageReader::for_response(&mut input, &mut body);
    let mut chunk = [0u8; 1024];
    let mut retries = 0u32;
    loop {
        match socket.read(&mut chunk) {
            Ok(0) => {
                return (
                    with_link(
                        format!(
                        "error the peer closed before the answer was complete retries={retries}"
                    ),
                        link,
                    ),
                    None,
                )
            }
            Ok(length) => match reader.feed(&chunk[..length]) {
                Ok(Progress::Complete) => break,
                Ok(Progress::NeedMore) => {}
                Err(error) => {
                    return (
                        with_link(format!("error {error} retries={retries}"), link),
                        None,
                    )
                }
            },
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                retries = retries.saturating_add(1);
                if started.elapsed() >= ANSWER_TIMEOUT {
                    return (
                        with_link(format!("error no answer retries={retries}"), link),
                        None,
                    );
                }
            }
            Err(error) => return (with_link(io_error(&error), link), None),
        }
    }
    let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);

    let response = match reader.response() {
        Ok(response) => response,
        Err(error) => return (with_link(format!("error {error}"), link), None),
    };
    let selected = selected_headers(&response.head.headers);
    let (bytes, text) = match response.body {
        Some(body) => (
            body.len().to_string(),
            String::from_utf8_lossy(body).into_owned(),
        ),
        None => ("none".to_string(), String::new()),
    };
    let total = text.chars().count();
    let content: String = text.chars().take(ANSWER_CHARS).collect();
    let mut result = format!(
        "status={} reason={} bytes={bytes} headers={selected} content={content} retries={retries} rt_us={micros}",
        response.head.status.as_u16(),
        response.head.reason_text(),
    );
    if total > ANSWER_CHARS {
        // The prompt of the next step carries this text, so the tool says what
        // it did not carry instead of silently cutting the body.
        let _ = write!(result, " omitted={}", total - ANSWER_CHARS);
    }
    (with_link(result, link), Some(RoundTrip { micros, retries }))
}

/// The headers of an answer that matter to a reader, as one token.
///
/// The list is the one a caller acts on: the type and the length of the body,
/// the coding, the server and a redirect. A header outside it is dropped before
/// the result goes back into the prompt; the full body is in the log anyway.
fn selected_headers(headers: &madrust_http::Headers<'_>) -> String {
    const WANTED: [&str; 5] = [
        "content-type",
        "content-length",
        "transfer-encoding",
        "server",
        "location",
    ];
    let mut out = String::new();
    for name in WANTED {
        if let Some(value) = headers.get(name) {
            if !out.is_empty() {
                out.push(',');
            }
            out.push_str(name);
            out.push(':');
            out.push_str(&String::from_utf8_lossy(value));
        }
    }
    if out.is_empty() {
        out.push('-');
    }
    out
}

/// Bytes of the request a `http_get` call writes.
const REQUEST_BYTES: usize = 1024;

/// Bytes of the head and the encoded body the HTTP tool collects.
const RESPONSE_BYTES: usize = 8192;

/// Bytes of the decoded body the HTTP tool keeps.
const BODY_BYTES: usize = 4096;

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
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

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
            br#"{"tool":"http_get","args":{"query":"http://10.0.2.2:8080/"}}"#,
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
    fn the_registry_lists_the_seven_tools() {
        assert_eq!(
            NAMES,
            [
                "read_file",
                "list_dir",
                "write_file",
                "compute",
                "net_send",
                "http_get",
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
            result.contains(" link="),
            "the answer carries the state of the link: {result}"
        );
        assert!(
            step.net.is_some(),
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
    fn a_failure_of_the_tool_still_carries_the_state_of_the_link() {
        // A send that fails on a kernel that refused the setup names both: the
        // error of the send alone would not say why no route existed.
        let link = crate::link::ensure();
        let result = super::with_link("error errno=101".to_string(), link);
        assert!(result.starts_with("error errno=101 link="), "{result}");
    }

    /// A server on the host that answers one HTTP request.
    ///
    /// The worker returns the bytes of the request it read, so the test can
    /// check the line and the headers the tool sent.
    fn http_once(reply: Vec<u8>) -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<u8>>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .expect("the test server binds a free port");
        let address = listener
            .local_addr()
            .expect("the test server has an address");
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("the client connects");
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("the test server waits for its request");
            let mut request = Vec::new();
            let mut buffer = [0u8; 512];
            loop {
                let length = socket.read(&mut buffer).expect("the request arrives");
                if length == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..length]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            socket.write_all(&reply).expect("the answer leaves");
            request
        });
        (address, worker)
    }

    #[test]
    fn requests_a_resource_and_reports_the_status_the_headers_and_the_body() {
        let reply = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 12\r\n\r\nhello world\n".to_vec();
        let (address, worker) = http_once(reply);
        let query = format!("http://{address}/index.html?q=1");
        let step = run(&call("http_get", &[("query", text(&query))]));
        let request = worker.join().expect("the test server finishes");
        let request = String::from_utf8_lossy(&request);
        assert!(
            request.starts_with("GET /index.html?q=1 HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains(&format!("Host: 127.0.0.1:{}\r\n", address.port())),
            "{request}"
        );
        assert!(
            request.contains("user-agent: madrust-agent/0.1\r\n"),
            "{request}"
        );
        let Outcome::Continue(result) = step.outcome else {
            panic!("http_get must not end the loop");
        };
        assert!(
            result.starts_with("status=200 reason=OK bytes=12 headers=content-type:text/plain,content-length:12 content=hello world"),
            "{result}"
        );
        assert!(result.contains(" rt_us="), "{result}");
        assert!(result.contains(" link="), "{result}");
        assert!(
            step.http.is_some(),
            "the tool measured its request: {result}"
        );
        assert!(step.net.is_none());
    }

    #[test]
    fn a_body_longer_than_the_window_says_how_much_was_left_out() {
        let body = "a".repeat(300);
        let reply = format!("HTTP/1.1 200 OK\r\nContent-Length: 300\r\n\r\n{body}").into_bytes();
        let (address, worker) = http_once(reply);
        let query = format!("http://{address}/");
        let step = run(&call("http_get", &[("query", text(&query))]));
        worker.join().expect("the test server finishes");
        let Outcome::Continue(result) = step.outcome else {
            panic!("http_get must not end the loop");
        };
        assert!(result.contains("omitted=44"), "{result}");
    }

    #[test]
    fn a_body_that_is_absent_is_not_a_body_of_zero_bytes() {
        let (address, worker) = http_once(b"HTTP/1.1 204 No Content\r\n\r\n".to_vec());
        let step = run(&call(
            "http_get",
            &[("query", text(&format!("http://{address}/")))] as &[(&str, Value)],
        ));
        worker.join().expect("the test server finishes");
        let Outcome::Continue(result) = step.outcome else {
            panic!("http_get must not end the loop");
        };
        assert!(result.contains("bytes=none"), "{result}");

        let (address, worker) = http_once(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec());
        let step = run(&call(
            "http_get",
            &[("query", text(&format!("http://{address}/")))],
        ));
        worker.join().expect("the test server finishes");
        let Outcome::Continue(result) = step.outcome else {
            panic!("http_get must not end the loop");
        };
        assert!(result.contains("bytes=0"), "{result}");
    }

    #[test]
    fn a_chunked_answer_is_decoded() {
        let reply = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n".to_vec();
        let (address, worker) = http_once(reply);
        let step = run(&call(
            "http_get",
            &[("query", text(&format!("http://{address}/")))],
        ));
        worker.join().expect("the test server finishes");
        let Outcome::Continue(result) = step.outcome else {
            panic!("http_get must not end the loop");
        };
        assert!(result.contains("content=Wikipedia"), "{result}");
    }

    #[test]
    fn a_bad_address_is_a_reason_and_not_a_default() {
        let step = run(&call(
            "http_get",
            &[("query", text("10.0.2.2:8080/index.html"))],
        ));
        assert!(step.http.is_none());
        assert!(matches!(
            step.outcome,
            Outcome::Continue(error) if error.starts_with("error the address is invalid")
        ));
    }

    #[test]
    fn a_bad_endpoint_is_a_reason_and_not_a_default() {
        let step = run(&call(
            "net_send",
            &[("query", text("10.0.2.2")), ("text", text("ping"))],
        ));
        assert!(step.net.is_none());
        assert!(matches!(
            step.outcome,
            Outcome::Continue(error) if error.starts_with("error endpoint is not ADDRESS:PORT")
        ));
    }
}
