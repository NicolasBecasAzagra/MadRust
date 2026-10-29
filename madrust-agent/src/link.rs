// SPDX-License-Identifier: GPL-2.0-only
//! The link of the guest: the interface the network tool sends from.
//!
//! MadRust brings the network card up with the address of the emulator, so a
//! program that sends at once has a route to the host. A mainline kernel with no
//! network configuration leaves the card down and unnumbered, so the datagram
//! has no route and the send answers `ENETUNREACH` (errno 101). That is the
//! difference the network tool of the agent saw: the same tool measured its
//! round trip on MadRust and failed on the reference kernel before it could
//! measure anything.
//!
//! The agent therefore configures the interface itself, with the same `ioctl`
//! calls the network client of the rig makes, once per process and before the
//! first datagram. The calls are best effort: a kernel that already configured
//! the interface needs none of them, and a kernel that does not implement them
//! answers with an error and keeps the configuration it already had. A refusal
//! is not a failure of the tool, so it is not an error; it is reported in the
//! result of the network call, because a kernel that refuses the calls and has
//! no address of its own leaves the send without a route, and the log has to
//! say why the round trip did not happen.

use std::sync::OnceLock;

/// Name of the interface of the guest on the user mode network of the emulator.
///
/// The device of the rig is a `virtio-net-pci` card, and both kernels name it
/// `eth0`: MadRust names the card it finds, and the reference kernel names it
/// that way because no program of the guest renames it. A kernel that names it
/// otherwise refuses the calls with `ENODEV`, and the report says so.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const INTERFACE: &str = "eth0";

/// What the setup of the interface did.
///
/// The state is one token, so it fits in the result line of the tool: the name
/// of the interface and `ok`, or the calls the kernel refused with their error
/// number, as in `eth0:addr=19,netmask=19,flags=19`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// One token that describes the result of the setup.
    state: String,
}

impl Link {
    /// The state of the link, as the tool puts it in its result.
    pub fn report(&self) -> &str {
        &self.state
    }
}

/// Configure the interface once, and report what every call did.
///
/// The first datagram of the process needs the link, so the first call of the
/// tool pays for it and every later call reads the same answer. Configuring it
/// again would not change the result of the run, and the report of a later call
/// would hide the first refusal.
pub fn ensure() -> &'static Link {
    static LINK: OnceLock<Link> = OnceLock::new();
    LINK.get_or_init(configure)
}

/// The result of a setup on a host that is not the guest.
///
/// The tests of this crate run on the machine of the developer, which already
/// has a configured interface, and the calls of an interface are not the same
/// ones there. Nothing is configured and the report says so.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn configure() -> Link {
    Link {
        state: "host".to_string(),
    }
}

/// Configure the interface with the `ioctl` calls of the Linux ABI.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn configure() -> Link {
    use std::fmt::Write as _;

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
        /// Open a socket and return its descriptor.
        fn socket(domain: i32, kind: i32, protocol: i32) -> i32;
        /// Configure a socket or an interface of the machine.
        fn ioctl(descriptor: i32, request: i32, argument: *mut u8) -> i32;
        /// Close a descriptor.
        fn close(descriptor: i32) -> i32;
    }

    let mut refused: Vec<String> = Vec::new();
    // SAFETY: the three calls are the ones the interface of a Linux machine
    // takes, and every block below is a live `ifreq`, the size the kernel
    // copies. A kernel that does not implement them answers with an error,
    // which is reported and not treated as a failure.
    unsafe {
        let descriptor = socket(AF_INET, SOCK_DGRAM, 0);
        if descriptor < 0 {
            return Link {
                state: format!("{INTERFACE}:socket={}", last_errno()),
            };
        }
        for (name, request, address) in [
            ("addr", SIOCSIFADDR, Some(GUEST_ADDRESS)),
            ("netmask", SIOCSIFNETMASK, Some(NETMASK)),
        ] {
            let mut block = [0u8; IFREQ_BYTES];
            block[IFREQ_NAME..IFREQ_NAME + INTERFACE.len()].copy_from_slice(INTERFACE.as_bytes());
            block[IFREQ_FAMILY..IFREQ_FAMILY + 2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
            if let Some(address) = address {
                block[IFREQ_ADDRESS..IFREQ_ADDRESS + 4].copy_from_slice(&address);
            }
            if ioctl(descriptor, request, block.as_mut_ptr()) != 0 {
                refused.push(format!("{name}={}", last_errno()));
            }
        }

        // The address alone does not start the interface, the flag does. The
        // flags come back in the same block, so the call reads them first and
        // keeps the ones the interface already had.
        let mut block = [0u8; IFREQ_BYTES];
        block[IFREQ_NAME..IFREQ_NAME + INTERFACE.len()].copy_from_slice(INTERFACE.as_bytes());
        if ioctl(descriptor, SIOCGIFFLAGS, block.as_mut_ptr()) != 0 {
            refused.push(format!("flags={}", last_errno()));
        } else {
            let flags = u16::from_ne_bytes([block[IFREQ_FLAGS], block[IFREQ_FLAGS + 1]]) | IFF_UP;
            block[IFREQ_FLAGS..IFREQ_FLAGS + 2].copy_from_slice(&flags.to_ne_bytes());
            if ioctl(descriptor, SIOCSIFFLAGS, block.as_mut_ptr()) != 0 {
                refused.push(format!("up={}", last_errno()));
            }
        }
        let _ = close(descriptor);
    }

    let mut state = String::new();
    if refused.is_empty() {
        let _ = write!(state, "{INTERFACE}:ok");
    } else {
        let _ = write!(state, "{INTERFACE}:{}", refused.join(","));
    }
    Link { state }
}

/// The number of the last failed call, as the kernel reported it.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn last_errno() -> i32 {
    // The standard library reads the error of the calling thread, which the
    // failed call has just written. A call that fails without an error number
    // is reported as zero rather than as a number this module invents.
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{ensure, Link};

    #[test]
    fn the_report_is_one_token() {
        let link = Link {
            state: "eth0:ok".to_string(),
        };
        assert_eq!(link.report(), "eth0:ok");
        assert!(!link.report().contains(char::is_whitespace));

        let refused = Link {
            state: "eth0:addr=25,netmask=25,flags=25".to_string(),
        };
        assert!(!refused.report().contains(char::is_whitespace));
        assert_eq!(refused.report(), "eth0:addr=25,netmask=25,flags=25");
    }

    #[test]
    fn the_setup_runs_once_and_reports_a_state() {
        let first = ensure();
        // The second call reads the result of the first instead of configuring
        // the interface again.
        assert!(std::ptr::eq(first, ensure()));
        assert!(!first.report().is_empty());
        assert!(!first.report().contains(char::is_whitespace));
    }
}
