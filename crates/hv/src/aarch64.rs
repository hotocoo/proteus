//! ARM64 pieces every backend shares: register file and exception syndrome decoding.

/// Registers the VMM reads or writes itself. Everything else stays in the hypervisor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Regs {
    pub x: [u64; GPR_COUNT],
    pub pc: u64,
    pub pstate: u64,
    pub sp_el1: u64,
}

pub const GPR_COUNT: usize = 31;

/// PSTATE for kernel entry: EL1 using SP_EL1, with D, A, I and F masked.
pub const PSTATE_EL1H_DAIF_MASKED: u64 = 0x3c5;

/// Width of an A64 instruction; the amount to step the PC past a trapped one.
pub const INSN_LEN: u64 = 4;

/// MPIDR_EL1 affinity for a vCPU index. GICv3 routes by it, and the FDT `cpus` node must match.
/// Aff0 holds 16 CPUs because an SGI target list is 16 bits wide.
pub fn mpidr_affinity(index: u32) -> u64 {
    let aff0 = u64::from(index) & AFF0_MASK;
    let aff1 = u64::from(index) >> AFF0_BITS;
    (aff1 << AFF1_SHIFT) | aff0
}

/// Inverse of `mpidr_affinity`: the vCPU index an affinity value names, if it names one.
pub fn cpu_index(affinity: u64) -> Option<u32> {
    let aff0 = affinity & AFF_FIELD_MASK;
    let aff1 = (affinity >> AFF1_SHIFT) & AFF_FIELD_MASK;
    let index = u32::try_from((aff1 << AFF0_BITS) | aff0).ok()?;
    (aff0 <= AFF0_MASK && mpidr_affinity(index) == affinity).then_some(index)
}

const AFF0_BITS: u32 = 4;
const AFF0_MASK: u64 = (1 << AFF0_BITS) - 1;
const AFF1_SHIFT: u32 = 8;
/// Width of one affinity level in MPIDR_EL1.
const AFF_FIELD_MASK: u64 = 0xff;

/// ESR_ELx fields, named after the Arm ARM.
pub mod esr {
    const EC_SHIFT: u32 = 26;
    const EC_MASK: u64 = 0x3f;

    pub const EC_WFX: u8 = 0x01;
    pub const EC_HVC64: u8 = 0x16;
    pub const EC_SMC64: u8 = 0x17;
    pub const EC_SYSREG: u8 = 0x18;
    pub const EC_DATA_ABORT_LOWER: u8 = 0x24;

    // Data abort ISS.
    const DA_ISV: u64 = 1 << 24;
    const DA_SAS_SHIFT: u32 = 22;
    const DA_SAS_MASK: u64 = 0x3;
    const DA_SSE: u64 = 1 << 21;
    const DA_SRT_SHIFT: u32 = 16;
    const DA_SRT_MASK: u64 = 0x1f;
    const DA_SF: u64 = 1 << 15;
    const DA_WNR: u64 = 1 << 6;

    // Trapped MSR/MRS ISS.
    const SYS_DIRECTION_READ: u64 = 1;
    const SYS_RT_SHIFT: u32 = 5;
    const SYS_RT_MASK: u64 = 0x1f;
    /// Everything in the ISS except the direction bit and Rt: Op0, Op2, Op1, CRn and CRm.
    const SYS_REGISTER_MASK: u64 = 0x3f_fc1e;

    const fn sysreg(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> SysReg {
        SysReg(op0 << 20 | op2 << 17 | op1 << 14 | crn << 10 | crm << 1)
    }

    /// A system register, identified by its encoding as it appears in the syndrome.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct SysReg(u64);

    /// OS lock registers. Linux clears both on every CPU it brings up; there is no debug state to lock.
    pub const OSLAR_EL1: SysReg = sysreg(2, 0, 1, 0, 4);
    pub const OSDLR_EL1: SysReg = sysreg(2, 0, 1, 3, 4);

    impl SysReg {
        /// Registers whose reads-as-zero, writes-ignored handling is known to be right.
        pub fn is_expected(self) -> bool {
            self == OSLAR_EL1 || self == OSDLR_EL1
        }
    }

    /// Register number that means "zero register" in a load/store or MSR/MRS.
    pub const REG_ZR: u8 = 31;

    pub fn class(esr: u64) -> u8 {
        ((esr >> EC_SHIFT) & EC_MASK) as u8
    }

    /// One guest load or store that faulted on unmapped memory.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MmioAccess {
        pub len: usize,
        pub write: bool,
        /// Source or destination register; `REG_ZR` discards.
        pub reg: u8,
        sign_extend: bool,
        to_64: bool,
    }

    /// `None` when the syndrome is not valid (ISV clear): the instruction was not a
    /// plain single-register load or store and cannot be emulated from the syndrome.
    pub fn decode_data_abort(esr: u64) -> Option<MmioAccess> {
        if esr & DA_ISV == 0 {
            return None;
        }
        Some(MmioAccess {
            len: 1 << ((esr >> DA_SAS_SHIFT) & DA_SAS_MASK),
            write: esr & DA_WNR != 0,
            reg: ((esr >> DA_SRT_SHIFT) & DA_SRT_MASK) as u8,
            sign_extend: esr & DA_SSE != 0,
            to_64: esr & DA_SF != 0,
        })
    }

    impl MmioAccess {
        /// Value to place in the destination register after a load returned `bytes`.
        pub fn load_result(&self, bytes: &[u8]) -> u64 {
            let mut raw = [0u8; 8];
            raw[..self.len].copy_from_slice(&bytes[..self.len]);
            let value = u64::from_le_bytes(raw);
            if !self.sign_extend {
                return value;
            }
            let unused = u64::BITS - (self.len as u32) * u8::BITS;
            let extended = ((value << unused) as i64 >> unused) as u64;
            if self.to_64 {
                extended
            } else {
                extended & u64::from(u32::MAX)
            }
        }
    }

    /// A trapped MSR/MRS: which system register, which general register, and the direction.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct SysRegAccess {
        pub sysreg: SysReg,
        /// Source or destination general register; `REG_ZR` discards.
        pub reg: u8,
        pub is_read: bool,
    }

    pub fn decode_sysreg(esr: u64) -> SysRegAccess {
        SysRegAccess {
            sysreg: SysReg(esr & SYS_REGISTER_MASK),
            reg: ((esr >> SYS_RT_SHIFT) & SYS_RT_MASK) as u8,
            is_read: esr & SYS_DIRECTION_READ != 0,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const IL: u64 = 1 << 25;

        fn abort(iss: u64) -> u64 {
            (u64::from(EC_DATA_ABORT_LOWER) << EC_SHIFT) | IL | iss
        }

        #[test]
        fn class_is_extracted() {
            assert_eq!(class(abort(0)), EC_DATA_ABORT_LOWER);
            assert_eq!(class(u64::from(EC_HVC64) << EC_SHIFT), EC_HVC64);
        }

        #[test]
        fn missing_syndrome_is_refused() {
            assert_eq!(decode_data_abort(abort(DA_WNR)), None);
        }

        #[test]
        fn widths_and_registers() {
            // strb w3, [..]
            let a = decode_data_abort(abort(DA_ISV | DA_WNR | 3 << DA_SRT_SHIFT)).unwrap();
            assert_eq!((a.len, a.write, a.reg), (1, true, 3));
            // ldr x7, [..]
            let a = decode_data_abort(abort(
                DA_ISV | DA_SF | 3 << DA_SAS_SHIFT | 7 << DA_SRT_SHIFT,
            ))
            .unwrap();
            assert_eq!((a.len, a.write, a.reg), (8, false, 7));
            assert_eq!(
                a.load_result(&0x1122_3344_5566_7788u64.to_le_bytes()),
                0x1122_3344_5566_7788
            );
        }

        #[test]
        fn zero_extension_ignores_high_bytes_of_buffer() {
            // ldrh w1, [..]
            let a =
                decode_data_abort(abort(DA_ISV | 1 << DA_SAS_SHIFT | 1 << DA_SRT_SHIFT)).unwrap();
            assert_eq!(a.load_result(&[0xfe, 0xff, 0xaa, 0xaa, 0, 0, 0, 0]), 0xfffe);
        }

        #[test]
        fn sign_extension_to_32_and_64() {
            // ldrsb w2, [..]: sign-extend to 32 bits, upper half clear
            let a = decode_data_abort(abort(DA_ISV | DA_SSE | 2 << DA_SRT_SHIFT)).unwrap();
            assert_eq!(a.load_result(&[0x80]), 0xffff_ff80);
            // ldrsw x2, [..]: sign-extend to 64 bits
            let a = decode_data_abort(abort(DA_ISV | DA_SSE | DA_SF | 2 << DA_SAS_SHIFT)).unwrap();
            assert_eq!(
                a.load_result(&0x8000_0001u32.to_le_bytes()),
                0xffff_ffff_8000_0001
            );
            // positive value is untouched
            assert_eq!(a.load_result(&0x7000_0001u32.to_le_bytes()), 0x7000_0001);
        }

        #[test]
        fn sysreg_direction_and_register() {
            let read = decode_sysreg(4 << SYS_RT_SHIFT | SYS_DIRECTION_READ);
            assert_eq!((read.reg, read.is_read), (4, true));
            let write = decode_sysreg(u64::from(REG_ZR) << SYS_RT_SHIFT);
            assert_eq!((write.reg, write.is_read), (REG_ZR, false));
        }

        /// Syndromes captured from Linux 6.1 clearing the OS locks under Hypervisor.framework.
        #[test]
        fn os_lock_writes_are_recognised() {
            let oslar = decode_sysreg(0x6228_07e0);
            assert_eq!(
                (oslar.sysreg, oslar.reg, oslar.is_read),
                (OSLAR_EL1, REG_ZR, false)
            );
            assert_eq!(decode_sysreg(0x6228_07e6).sysreg, OSDLR_EL1);
            assert!(oslar.sysreg.is_expected());
            // GKI 6.12 writes the same registers from x8.
            let gki = decode_sysreg(0x6228_0506);
            assert_eq!((gki.sysreg, gki.reg), (OSDLR_EL1, 8));
            assert_eq!(decode_sysreg(0x6228_0500).sysreg, OSLAR_EL1);
            assert!(!decode_sysreg(0).sysreg.is_expected());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_wraps_into_aff1_after_sixteen() {
        assert_eq!(mpidr_affinity(0), 0);
        assert_eq!(mpidr_affinity(15), 15);
        assert_eq!(mpidr_affinity(16), 1 << AFF1_SHIFT);
        assert_eq!(mpidr_affinity(17), 1 << AFF1_SHIFT | 1);
    }

    #[test]
    fn cpu_index_inverts_affinity_and_rejects_values_no_vcpu_has() {
        for index in [0, 1, 15, 16, 17, 63] {
            assert_eq!(cpu_index(mpidr_affinity(index)), Some(index));
        }
        // Aff0 above 15 is never handed out, and neither are Aff2/Aff3.
        assert_eq!(cpu_index(16), None);
        assert_eq!(cpu_index(1 << 16), None);
        assert_eq!(cpu_index(1 << 32), None);
    }
}
