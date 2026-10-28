// SPDX-License-Identifier: GPL-2.0-only
//! Network round trip client.
//!
//! One static binary for the Linux musl target holds the whole program, so the
//! exact same bytes run on MadRust and on mainline Linux. It opens a datagram
//! socket, connects it to the server the rig runs on the host, sends one short
//! datagram per sample, waits for the echo of the server and prints one line
//! per sample:
//!
//! ```text
//! NET OK rt=<microseconds> retries=<n> answer=<text>
//! ```
//!
//! `rt` is the round trip of the datagram: the clock of the library starts
//! after `send` returned and stops when the answer is in hand. `retries` counts
//! the receive attempts that found nothing yet, because the guest and the
//! server do not run at the same speed, and it also counts answers to an
//! earlier sample, which are read and dropped. `answer` is the text the server
//! sent back, and the program only accepts an answer that is the datagram it
//! just sent.
//!
//! A sample that never arrives ends the run with
//! `NET FAIL stage=<step> ...`, so a failure is a line with a reason and never
//! a zero. The last line of a good run is `NET DONE`.
//!
//! ## Where the address and the port come from
//!
//! The program takes them as arguments, `madrust-netclient [ADDRESS] [PORT]
//! [COUNT]`, so it can also be run by hand on any Linux. Neither kernel hands
//! arguments to the program it starts as init: MadRust builds an argument
//! vector that holds the name of the program alone, and mainline Linux execs
//! `/init` without arguments. The rig therefore writes the same two values,
//! plus the number of samples, into `/net.conf` of the initramfs:
//!
//! ```text
//! address=10.0.2.2
//! port=43512
//! count=5
//! ```
//!
//! An argument wins over the file, the file wins over the defaults.
//!
//! ## The link of mainline
//!
//! MadRust brings its card up with the address of the emulator. A mainline
//! kernel with no network configuration leaves the interface down and
//! unnumbered, so no route to the host exists and a datagram has nowhere to go.
//! The program therefore configures the interface itself with `ioctl`, best
//! effort: MadRust answers those calls of the interface with an error and needs
//! none of them, and a kernel that refuses them is not a failure of the round
//! trip, which is reported by the sample lines instead.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// Path of the configuration file inside the initramfs.
const CONFIG_PATH: &str = "/net.conf";

/// Address of the host on the user mode network of the emulator.
const DEFAULT_ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// Port of the server when nothing else is asked for.
const DEFAULT_PORT: u16 = 9999;

/// Samples of one run when nothing else is asked for.
const DEFAULT_COUNT: usize = 1;

/// How long one sample waits for its answer.
///
/// The wait is a poll of the socket and not a sleep. MadRust hands a datagram to
/// the program when the program reads the card, so a program that slept would
/// add the granularity of the timer, up to one tick of ten milliseconds, to the
/// round trip it is measuring. A kernel that blocks inside the read waits there
/// instead, and the loop only comes back when the answer is still missing.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(1);

/// Wait of one attempt on a kernel that honours the timeout of the socket.
const READ_TIMEOUT: Duration = Duration::from_millis(100);

/// Largest answer the program reads.
const BUFFER_BYTES: usize = 256;

/// Address of the guest on the user mode network of the emulator.
const GUEST_ADDRESS: [u8; 4] = [10, 0, 2, 15];

/// Mask of that network, twenty four bits.
const NETMASK: [u8; 4] = [255, 255, 255, 0];

/// `ifreq` of an interface request, the size the kernel copies.
const IFREQ_BYTES: usize = 40;
/// Offset of the name of the interface in `ifreq`.
const IFREQ_NAME: usize = 0;
/// Offset of the flags of `ifreq`, which is the head of its union.
const IFREQ_FLAGS: usize = 16;
/// Offset of `sockaddr_in.sin_family` in the union of `ifreq`.
const IFREQ_FAMILY: usize = 16;
/// Offset of `sockaddr_in.sin_addr` in the union of `ifreq`.
const IFREQ_ADDRESS: usize = 20;

/// `AF_INET`.
const AF_INET: i32 = 2;
/// `SOCK_DGRAM`.
const SOCK_DGRAM: i32 = 2;
/// `IFF_UP`.
const IFF_UP: u16 = 0x1;

const SIOCGIFFLAGS: i32 = 0x8913;
const SIOCSIFFLAGS: i32 = 0x8914;
const SIOCSIFADDR: i32 = 0x8916;
const SIOCSIFNETMASK: i32 = 0x891c;

extern "C" {
    /// Configure a socket or an interface of the machine.
    fn ioctl(descriptor: i32, request: i32, argument: *mut u8) -> i32;
    /// Open a socket and return its descriptor.
    fn socket(domain: i32, kind: i32, protocol: i32) -> i32;
    /// Close a descriptor.
    fn close(descriptor: i32) -> i32;
}

/// How the program reaches the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Config {
    /// Address of the server.
    address: Ipv4Addr,
    /// Port of the server.
    port: u16,
    /// Samples this run takes.
    count: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            address: DEFAULT_ADDRESS,
            port: DEFAULT_PORT,
            count: DEFAULT_COUNT,
        }
    }
}

impl Config {
    /// Read the configuration file, then let the arguments override it.
    ///
    /// A missing file is not an error: a run by hand on a normal Linux has no
    /// initramfs to put one in.
    fn load() -> Result<Self, String> {
        let mut config = match std::fs::read_to_string(CONFIG_PATH) {
            Ok(text) => Self::from_text(&text)?,
            Err(_) => Self::default(),
        };
        let arguments: Vec<String> = std::env::args().skip(1).collect();
        config.overlay(&arguments)?;
        Ok(config)
    }

    /// Parse the `key=value` lines of the configuration file.
    fn from_text(text: &str) -> Result<Self, String> {
        let mut config = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("stage=config error=line without a key: {line}"));
            };
            let value = value.trim();
            match key.trim() {
                "address" => config.address = parse_address(value)?,
                "port" => config.port = parse_port(value)?,
                "count" => config.count = parse_count(value)?,
                other => return Err(format!("stage=config error=unknown key: {other}")),
            }
        }
        config.check()
    }

    /// Let the arguments of the program override the file.
    fn overlay(&mut self, arguments: &[String]) -> Result<(), String> {
        if let Some(value) = arguments.first() {
            self.address = parse_address(value)?;
        }
        if let Some(value) = arguments.get(1) {
            self.port = parse_port(value)?;
        }
        if let Some(value) = arguments.get(2) {
            self.count = parse_count(value)?;
        }
        self.check().map(|_| ())
    }

    /// Reject a configuration that would make the run say nothing.
    fn check(self) -> Result<Self, String> {
        if self.count == 0 {
            return Err("stage=config error=count must be positive".to_string());
        }
        Ok(self)
    }
}

/// Parse the address of the server.
fn parse_address(value: &str) -> Result<Ipv4Addr, String> {
    value
        .parse()
        .map_err(|_| format!("stage=config error=address: {value}"))
}

/// Parse the port of the server.
fn parse_port(value: &str) -> Result<u16, String> {
    value
        .parse()
        .map_err(|_| format!("stage=config error=port: {value}"))
}

/// Parse the number of samples.
fn parse_count(value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("stage=config error=count: {value}"))
}

/// Send `payload` and wait for the echo of the server.
///
/// Returns the answer and the number of attempts it took. An answer that is not
/// the datagram just sent belongs to an earlier sample and is dropped, so a
/// late answer is never measured as the round trip of a later one. The read is
/// repeated until the answer is in hand or the sample is out of time, which is
/// the retry the line reports.
fn await_answer(socket: &UdpSocket, payload: &[u8]) -> Result<(String, u32), String> {
    let mut buffer = [0u8; BUFFER_BYTES];
    let mut retries = 0u32;
    let started = Instant::now();
    while started.elapsed() < ANSWER_TIMEOUT {
        match socket.recv(&mut buffer) {
            Ok(length) => {
                let answer = String::from_utf8_lossy(&buffer[..length]).into_owned();
                if answer.as_bytes() == payload {
                    return Ok((answer, retries));
                }
                retries += 1;
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                retries += 1;
            }
            Err(error) => return Err(format!("error={error}")),
        }
    }
    Err(format!("retries={retries} error=no answer"))
}

/// Bring the interface of the guest up, best effort.
///
/// The numbers are the ones of the user mode network of the emulator. A kernel
/// that already configured the interface, and a kernel that does not implement
/// the calls of the interface, both leave this as a no-op. The result is not
/// part of a measurement, so a refusal is not reported: the sample lines say
/// what the round trip did.
fn configure_link() {
    const NAME: &[u8] = b"eth0";
    // SAFETY: the three calls are the ones the interface of a Linux machine
    // takes, and every buffer below is a live `ifreq`, the size the kernel
    // copies. A kernel that does not implement them answers with an error,
    // which is ignored on purpose.
    unsafe {
        let descriptor = socket(AF_INET, SOCK_DGRAM, 0);
        if descriptor < 0 {
            return;
        }
        for (request, address) in [(SIOCSIFADDR, GUEST_ADDRESS), (SIOCSIFNETMASK, NETMASK)] {
            let mut block = [0u8; IFREQ_BYTES];
            block[IFREQ_NAME..IFREQ_NAME + NAME.len()].copy_from_slice(NAME);
            block[IFREQ_FAMILY..IFREQ_FAMILY + 2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
            block[IFREQ_ADDRESS..IFREQ_ADDRESS + 4].copy_from_slice(&address);
            let _ = ioctl(descriptor, request, block.as_mut_ptr());
        }

        // The address alone does not start the interface; the flag does.
        let mut block = [0u8; IFREQ_BYTES];
        block[IFREQ_NAME..IFREQ_NAME + NAME.len()].copy_from_slice(NAME);
        if ioctl(descriptor, SIOCGIFFLAGS, block.as_mut_ptr()) == 0 {
            let flags = u16::from_ne_bytes([block[IFREQ_FLAGS], block[IFREQ_FLAGS + 1]]) | IFF_UP;
            block[IFREQ_FLAGS..IFREQ_FLAGS + 2].copy_from_slice(&flags.to_ne_bytes());
            let _ = ioctl(descriptor, SIOCSIFFLAGS, block.as_mut_ptr());
        }
        let _ = close(descriptor);
    }
}

/// Send one datagram and wait for its echo, without reporting it.
///
/// The first datagram of a socket resolves the hardware address of the host and
/// brings the link up, which is a cost of the start of the run and not of the
/// round trip being measured. The warm up takes it before the samples begin,
/// the way the first round of the futex workload does.
fn warm_up(socket: &UdpSocket) -> Result<(), String> {
    let payload = b"ping-warmup";
    socket
        .send(payload)
        .map_err(|error| format!("stage=warmup error={error}"))?;
    await_answer(socket, payload)
        .map(|_| ())
        .map_err(|reason| format!("stage=warmup {reason}"))
}

/// Print the reason of a failed run and leave a line behind.
///
/// The machine is asked to go down first: mainline Linux panics when its PID 1
/// leaves, and the panic would bury the reason. MadRust answers the call with an
/// error and the program leaves the ordinary way.
fn fail(reason: &str) -> ! {
    println!("NET FAIL {reason}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    power_off();
    std::process::exit(1)
}

/// Ask the machine to go down, so the kernel does not panic when init leaves.
///
/// The call is a system call of the x86_64 Linux ABI. The guest is always a
/// machine of that architecture; a build for another one only exists for the
/// tests of this crate on the host of the developer, and it powers nothing off.
fn power_off() {
    #[cfg(target_arch = "x86_64")]
    {
        const SYS_REBOOT: u64 = 169;
        /// `LINUX_REBOOT_MAGIC1`: the first word the kernel checks.
        const MAGIC1: u64 = 0x0123_4567;
        /// `LINUX_REBOOT_MAGIC2`: the second word the kernel checks.
        const MAGIC2: u64 = 0xCDEF_0123;
        /// `LINUX_REBOOT_CMD_POWER_OFF`: the command that stops the machine.
        const POWER_OFF: u64 = 0x4321_FEDC;

        // SAFETY: reboot takes three arguments. A kernel that accepts the call
        // never returns from it; a kernel that refuses it returns an error
        // number and the caller continues.
        unsafe {
            let result: i64;
            core::arch::asm!(
                "syscall",
                inlateout("rax") SYS_REBOOT as i64 => result,
                in("rdi") MAGIC1,
                in("rsi") MAGIC2,
                in("rdx") POWER_OFF,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
            let _ = result;
        }
    }
}

fn main() {
    let config = match Config::load() {
        Ok(config) => config,
        Err(reason) => fail(&reason),
    };
    configure_link();

    // A port of zero lets the kernel pick a free one, which is what both
    // kernels do for the first datagram of a socket of their own.
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        Ok(socket) => socket,
        Err(error) => fail(&format!("stage=bind error={error}")),
    };
    // A kernel that does not implement the timeout of a socket refuses the
    // option; the loop over the attempts covers both behaviours.
    let _ = socket.set_read_timeout(Some(READ_TIMEOUT));

    let peer = SocketAddr::from((config.address, config.port));
    if let Err(error) = socket.connect(peer) {
        fail(&format!("stage=connect error={error}"));
    }
    if let Err(reason) = warm_up(&socket) {
        fail(&reason);
    }

    for index in 0..config.count {
        // The index is in the datagram, so the answer of the server names the
        // sample it belongs to and a late answer cannot be taken for this one.
        let payload = format!("ping-{index}");
        let started = Instant::now();
        if let Err(error) = socket.send(payload.as_bytes()) {
            fail(&format!("stage=send error={error}"));
        }
        let (answer, retries) = match await_answer(&socket, payload.as_bytes()) {
            Ok(answer) => answer,
            Err(reason) => fail(&format!("stage=answer {reason}")),
        };
        let micros = started.elapsed().as_micros();
        println!("NET OK rt={micros} retries={retries} answer={answer}");
    }

    println!("NET DONE");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    power_off();
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_host_of_the_emulator() {
        let config = Config::from_text("").expect("an empty file is a valid configuration");
        assert_eq!(config, Config::default());
        assert_eq!(config.address, Ipv4Addr::new(10, 0, 2, 2));
        assert_eq!(config.port, 9999);
        assert_eq!(config.count, 1);
    }

    #[test]
    fn the_file_carries_the_endpoint_and_the_sample_count() {
        let config =
            Config::from_text("# written by the rig\naddress = 10.0.2.2\nport=43512\n\ncount=5\n")
                .expect("the file is valid");
        assert_eq!(config.address, Ipv4Addr::new(10, 0, 2, 2));
        assert_eq!(config.port, 43512);
        assert_eq!(config.count, 5);
    }

    #[test]
    fn an_argument_wins_over_the_file() {
        let mut config =
            Config::from_text("address=10.0.2.2\nport=9999\ncount=5\n").expect("the file is valid");
        config
            .overlay(&["10.0.2.3".to_string(), "1234".to_string()])
            .expect("the arguments are valid");
        assert_eq!(config.address, Ipv4Addr::new(10, 0, 2, 3));
        assert_eq!(config.port, 1234);
        assert_eq!(config.count, 5);
    }

    #[test]
    fn a_bad_value_is_an_error_and_not_a_default() {
        assert!(Config::from_text("port=99999\n").is_err());
        assert!(Config::from_text("speed=1\n").is_err());
        assert!(Config::from_text("address\n").is_err());
        assert!(Config::from_text("count=0\n").is_err());
        let mut config = Config::default();
        assert!(config
            .overlay(&["10.0.2.2".to_string(), "0".to_string()])
            .is_ok());
        assert!(Config::default()
            .overlay(&["not-an-address".to_string()])
            .is_err());
    }

    #[test]
    fn the_payload_of_a_sample_names_the_sample() {
        let payload = format!("ping-{}", 3);
        assert_eq!(payload, "ping-3");
    }
}
