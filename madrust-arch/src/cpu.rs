// SPDX-License-Identifier: GPL-2.0-only
//! CPU control and identification helpers.

#[cfg(target_arch = "x86_64")]
mod imp {
    use core::arch::asm;

    /// Result of a `CPUID` instruction.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Cpuid {
        pub eax: u32,
        pub ebx: u32,
        pub ecx: u32,
        pub edx: u32,
    }

    impl Cpuid {
        /// The 12-byte vendor identification string, e.g. `b"GenuineIntel"`.
        pub const fn vendor(&self) -> [u8; 12] {
            let ebx = self.ebx.to_le_bytes();
            let edx = self.edx.to_le_bytes();
            let ecx = self.ecx.to_le_bytes();
            [
                ebx[0], ebx[1], ebx[2], ebx[3], edx[0], edx[1], edx[2], edx[3], ecx[0], ecx[1],
                ecx[2], ecx[3],
            ]
        }

        /// `true` when CPUID reports the hypervisor-present bit.
        pub const fn hypervisor_present(&self) -> bool {
            self.ecx & (1 << 31) != 0
        }
    }

    /// Execute `CPUID` with an explicit subleaf.
    pub fn cpuid_subleaf(leaf: u32, subleaf: u32) -> Cpuid {
        let (eax, ebx, ecx, edx): (u32, u32, u32, u32);
        // SAFETY: `cpuid` is available on every x86_64 CPU, reads no memory and
        // only writes the four general purpose registers listed as outputs.
        // `rbx` is reserved by LLVM, so it is preserved around the instruction
        // by hand: the value is saved to a scratch register, CPUID overwrites
        // ebx, the result is copied out, and rbx is restored.
        unsafe {
            asm!(
                "mov {saved}, rbx",
                "cpuid",
                "mov {ebx_out:e}, ebx",
                "mov rbx, {saved}",
                saved = lateout(reg) _,
                ebx_out = lateout(reg) ebx,
                inout("eax") leaf => eax,
                inout("ecx") subleaf => ecx,
                out("edx") edx,
                options(nomem, nostack),
            );
        }
        Cpuid { eax, ebx, ecx, edx }
    }

    /// Execute `CPUID` on a leaf, ignoring subleaves.
    pub fn cpuid(leaf: u32) -> Cpuid {
        cpuid_subleaf(leaf, 0)
    }

    /// Read CR2, the faulting linear address of the last page fault.
    pub fn read_cr2() -> u64 {
        let value: u64;
        // SAFETY: `mov` from CR2 only reads a control register into a general
        // purpose register and has no memory side effects.
        unsafe {
            asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// Read a model specific register.
    pub fn read_msr(register: u32) -> u64 {
        let low: u32;
        let high: u32;
        // SAFETY: `rdmsr` reads the register named in ECX into EDX:EAX. It has
        // no memory side effects and is available on every x86_64 CPU. The
        // caller must pass a register the CPU implements; a reserved one would
        // raise #GP.
        unsafe {
            asm!(
                "rdmsr",
                in("ecx") register,
                out("eax") low,
                out("edx") high,
                options(nomem, nostack, preserves_flags),
            );
        }
        (u64::from(high) << 32) | u64::from(low)
    }

    /// Write a model specific register.
    ///
    /// # Safety
    ///
    /// The caller must pass a register the CPU implements and a value that is
    /// valid for it: a reserved register raises #GP, and a bad value can change
    /// how the CPU translates addresses or handles system calls.
    pub unsafe fn write_msr(register: u32, value: u64) {
        // SAFETY: the caller guarantees that `register` exists and that `value`
        // is valid for it, which is the only requirement of `wrmsr`.
        unsafe {
            asm!(
                "wrmsr",
                in("ecx") register,
                in("eax") value as u32,
                in("edx") (value >> 32) as u32,
                options(nomem, nostack, preserves_flags),
            );
        }
    }

    /// Read CR3, the physical address of the top level page table.
    pub fn read_cr3() -> u64 {
        let value: u64;
        // SAFETY: `mov` from CR3 only reads a control register into a general
        // purpose register and has no memory side effects.
        unsafe {
            asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// Drop the cached translation of one page.
    pub fn invalidate_page(address: u64) {
        // SAFETY: `invlpg` only removes the entry for `address` from the TLB.
        // It does not touch memory and is valid on every x86_64 CPU.
        unsafe {
            asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags));
        }
    }

    /// Reload CR3, which discards every cached translation.
    ///
    /// # Safety
    ///
    /// The caller must have finished every change to the active page tables
    /// that the current instruction stream depends on.
    pub unsafe fn flush_tlb() {
        // SAFETY: the caller made the page tables consistent; reloading the
        // unchanged CR3 value only drops stale TLB entries.
        unsafe {
            asm!(
                "mov {scratch}, cr3",
                "mov cr3, {scratch}",
                scratch = out(reg) _,
                options(nomem, nostack, preserves_flags),
            );
        }
    }

    /// Halt the CPU until the next interrupt.
    pub fn hlt() {
        // SAFETY: `hlt` only stops instruction retirement; it has no memory
        // effects and no control flow of its own.
        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }

    /// Clear the interrupt flag.
    pub fn disable_interrupts() {
        // SAFETY: `cli` only modifies the interrupt flag in RFLAGS.
        unsafe {
            asm!("cli", options(nomem, nostack));
        }
    }

    /// Set the interrupt flag.
    pub fn enable_interrupts() {
        // SAFETY: `sti` only modifies the interrupt flag in RFLAGS.
        unsafe {
            asm!("sti", options(nomem, nostack));
        }
    }

    /// Stop this CPU forever.
    pub fn halt_loop() -> ! {
        loop {
            hlt();
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod imp {
    /// Host stub mirroring [`Cpuid`] from the x86_64 implementation.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Cpuid {
        pub eax: u32,
        pub ebx: u32,
        pub ecx: u32,
        pub edx: u32,
    }

    impl Cpuid {
        /// Host stub: reports an empty vendor string.
        pub const fn vendor(&self) -> [u8; 12] {
            [0; 12]
        }

        /// Host stub: never reports a hypervisor.
        pub const fn hypervisor_present(&self) -> bool {
            false
        }
    }

    /// Host stub: returns an all-zero result.
    pub fn cpuid_subleaf(_leaf: u32, _subleaf: u32) -> Cpuid {
        Cpuid {
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
        }
    }

    /// Host stub: returns an all-zero result.
    pub fn cpuid(_leaf: u32) -> Cpuid {
        cpuid_subleaf(0, 0)
    }

    /// Host stub: reports zero bytes read.
    pub fn read_cr2() -> u64 {
        0
    }

    /// Host stub: reports a zero register value.
    pub fn read_msr(_register: u32) -> u64 {
        0
    }

    /// Host stub: drops the write.
    ///
    /// # Safety
    ///
    /// The host has no model specific registers, so nothing can be corrupted.
    pub unsafe fn write_msr(_register: u32, _value: u64) {}

    /// Host stub: reports a zero top level page table.
    pub fn read_cr3() -> u64 {
        0
    }

    /// Host stub: drops the invalidate.
    ///
    /// # Safety
    ///
    /// The host has no page tables to invalidate.
    pub unsafe fn flush_tlb() {}

    /// Host stub: does nothing.
    pub fn invalidate_page(_address: u64) {}

    pub fn hlt() {}

    /// Host stub: does nothing.
    pub fn disable_interrupts() {}

    /// Host stub: does nothing.
    pub fn enable_interrupts() {}

    /// Host stub: spins forever.
    pub fn halt_loop() -> ! {
        loop {
            core::hint::spin_loop();
        }
    }
}

pub use imp::{
    cpuid, cpuid_subleaf, disable_interrupts, enable_interrupts, flush_tlb, halt_loop, hlt,
    invalidate_page, read_cr2, read_cr3, read_msr, write_msr, Cpuid,
};
