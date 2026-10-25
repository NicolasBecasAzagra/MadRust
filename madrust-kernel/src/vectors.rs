// SPDX-License-Identifier: GPL-2.0-only
//! Interrupt vector layout (milestone E3b).
//!
//! The first 32 vectors belong to the CPU exceptions. The 8259 PIC stays on
//! its historical `0x20..0x2F` range for machines that have no local APIC.
//! New sources take ranges that cannot collide with either, or with the
//! spurious vector the local APIC uses:
//!
//! | Vector | Source |
//! |--------|--------|
//! | `0x00..0x1F` | CPU exceptions |
//! | `0x20..0x2F` | 8259 PIC (legacy timer and device lines) |
//! | `0x30` | PIT routed through the input output APIC |
//! | `0x40` | local APIC timer of the running processor |
//! | `0x50` | inter processor interrupt: reschedule |
//! | `0x51` | inter processor interrupt: flush the translation cache |
//! | `0xFF` | local APIC spurious interrupt |

/// Vector the PIT uses when the input output APIC routes its line.
pub const PIT_IOAPIC_VECTOR: u8 = 0x30;

/// Vector of the local APIC timer that drives preemption.
pub const LAPIC_TIMER_VECTOR: u8 = 0x40;

/// Inter processor interrupt that makes a processor run its scheduler.
///
/// Sent when a thread is pushed into another processor's run queue and when
/// that processor is idle, so the wakeup does not wait for the idle timer.
pub const IPI_RESCHEDULE_VECTOR: u8 = 0x50;

/// Inter processor interrupt that reloads CR3 on every other processor.
pub const IPI_TLB_VECTOR: u8 = 0x51;

/// Spurious vector of the local APIC.
pub const SPURIOUS_VECTOR: u8 = 0xFF;
