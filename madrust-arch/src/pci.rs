// SPDX-License-Identifier: GPL-2.0-only
//! PCI configuration space addressing and register decoding.
//!
//! The guest has no firmware, so the kernel reads the configuration space of
//! the bus itself through the pair of ports the host bridge of the `q35`
//! machine exposes: [`CONFIG_ADDRESS`] selects a register and [`CONFIG_DATA`]
//! carries the 32-bit value. The address written to the first port holds the
//! enable bit, the bus, the device, the function and the register offset.
//!
//! Everything here is pure arithmetic so it can be checked on the host; the
//! port access stays in [`crate::port`].

/// Port that selects the configuration register to access.
pub const CONFIG_ADDRESS: u16 = 0xcf8;

/// Port that carries the data of the selected configuration register.
pub const CONFIG_DATA: u16 = 0xcfc;

/// Bit 31 of [`CONFIG_ADDRESS`]: turns the configuration access on.
pub const ENABLE: u32 = 0x8000_0000;

/// Vendor id of a function that is not present: register 0 reads all ones.
pub const NO_VENDOR: u16 = 0xffff;

/// Size of the configuration space of one function, in bytes.
pub const SPACE_SIZE: u16 = 256;

/// Build the value to write to [`CONFIG_ADDRESS`] for one register.
///
/// Each function owns 256 bytes of configuration space, so the two low bits of
/// `offset` are dropped: a register is always read as a whole 32-bit word.
pub const fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    ENABLE
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xfc)
}

/// Vendor id of a function: the low word of configuration register 0.
pub const fn vendor_id(register: u32) -> u16 {
    (register & 0xffff) as u16
}

/// Device id of a function: the high word of configuration register 0.
pub const fn device_id(register: u32) -> u16 {
    (register >> 16) as u16
}

/// Class code of a function: the top three bytes of register `0x08`.
pub const fn class_code(register: u32) -> u32 {
    register >> 8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The address always enables the configuration mechanism.
    #[test]
    fn address_sets_the_enable_bit() {
        for bus in 0..=u8::MAX {
            for device in 0..32 {
                for function in 0..8 {
                    for offset in 0..SPACE_SIZE {
                        let address = config_address(bus, device, function, offset as u8);
                        assert_eq!(address & ENABLE, ENABLE);
                    }
                }
            }
        }
    }

    #[test]
    fn address_of_the_first_register_of_the_bridge() {
        assert_eq!(config_address(0, 0x1f, 0, 0), 0x8000_f800);
    }

    #[test]
    fn address_carries_bus_device_and_function() {
        assert_eq!(config_address(1, 0, 0, 0), 0x8001_0000);
        assert_eq!(config_address(0, 2, 0, 0), 0x8000_1000);
        assert_eq!(config_address(0, 0, 1, 0), 0x8000_0100);
        assert_eq!(config_address(0xff, 0x1f, 7, 0), 0x80ff_ff00);
    }

    /// The four functions of one device share the device lane and differ in
    /// the function lane only.
    #[test]
    fn the_four_functions_of_a_device() {
        let base = config_address(0, 3, 0, 0) & !0x700;
        for function in 0..4 {
            assert_eq!(
                config_address(0, 3, function, 0),
                base | ((function as u32) << 8)
            );
        }
    }

    /// Every register of the space has its own aligned address, and the low
    /// two bits of the offset never reach the address.
    #[test]
    fn address_of_every_register_of_the_space() {
        let mut expected = config_address(0, 1, 0, 0);
        for offset in (0..SPACE_SIZE).step_by(4) {
            let address = config_address(0, 1, 0, offset as u8);
            assert_eq!(address, expected);
            for low in 0..4 {
                assert_eq!(config_address(0, 1, 0, offset as u8 + low), address);
            }
            expected += 4;
        }
        assert_eq!(expected, config_address(0, 1, 1, 0));
    }

    #[test]
    fn vendor_and_device_are_the_two_words_of_register_zero() {
        assert_eq!(vendor_id(0x29c0_8086), 0x8086);
        assert_eq!(device_id(0x29c0_8086), 0x29c0);
        assert_eq!(vendor_id(0x1000_1af4), 0x1af4);
        assert_eq!(device_id(0x1000_1af4), 0x1000);
    }

    #[test]
    fn an_empty_slot_reads_the_no_vendor_marker() {
        assert_eq!(vendor_id(u32::MAX), NO_VENDOR);
    }

    #[test]
    fn class_code_drops_the_revision_byte() {
        assert_eq!(class_code(0x0600_0000), 0x0006_0000);
        assert_eq!(class_code(0x0200_0001), 0x0002_0000);
    }
}
