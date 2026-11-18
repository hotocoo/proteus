//! PSCI 0.2 over HVC (Arm DEN 0022): CPU bring-up, power off and reset.
//!
//! Version 0.2 is reported on purpose: the guest then skips the feature and SMCCC probing a 1.0
//! implementation would have to answer.

use hv::Regs;
use hv::aarch64::cpu_index;

const FN_VERSION: u64 = 0x8400_0000;
const FN_CPU_OFF: u64 = 0x8400_0002;
const FN_CPU_ON: u64 = 0xc400_0003;
const FN_AFFINITY_INFO: u64 = 0xc400_0004;
const FN_MIGRATE_INFO_TYPE: u64 = 0x8400_0006;
const FN_SYSTEM_OFF: u64 = 0x8400_0008;
const FN_SYSTEM_RESET: u64 = 0x8400_0009;

/// The function ID is a 32-bit value in `w0`.
const FN_ID_MASK: u64 = 0xffff_ffff;

const VERSION_0_2: u64 = 2;
/// `MIGRATE_INFO_TYPE` answer: no trusted OS, migration never needed.
const MIGRATE_NOT_REQUIRED: u64 = 2;

pub const SUCCESS: u64 = 0;
pub const NOT_SUPPORTED: u64 = -1i64 as u64;
pub const INVALID_PARAMETERS: u64 = -2i64 as u64;
pub const ALREADY_ON: u64 = -4i64 as u64;
/// `AFFINITY_INFO` answers.
pub const AFFINITY_ON: u64 = 0;
pub const AFFINITY_OFF: u64 = 1;

/// What a hypercall asks the VMM to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Call {
    /// Nothing to do but hand this value back in `x0`.
    Return(u64),
    CpuOn {
        cpu: u32,
        entry: u64,
        context: u64,
    },
    /// The calling CPU stops for good.
    CpuOff,
    /// Answer `AFFINITY_ON` or `AFFINITY_OFF` for this CPU.
    AffinityInfo {
        cpu: u32,
    },
    SystemOff,
    SystemReset,
}

pub fn decode(regs: &Regs) -> Call {
    let target = || cpu_index(regs.x[1]);
    match regs.x[0] & FN_ID_MASK {
        FN_VERSION => Call::Return(VERSION_0_2),
        FN_MIGRATE_INFO_TYPE => Call::Return(MIGRATE_NOT_REQUIRED),
        FN_CPU_ON => match target() {
            Some(cpu) => Call::CpuOn {
                cpu,
                entry: regs.x[2],
                context: regs.x[3],
            },
            None => Call::Return(INVALID_PARAMETERS),
        },
        FN_AFFINITY_INFO => target().map_or(Call::Return(INVALID_PARAMETERS), |cpu| {
            Call::AffinityInfo { cpu }
        }),
        FN_CPU_OFF => Call::CpuOff,
        FN_SYSTEM_OFF => Call::SystemOff,
        FN_SYSTEM_RESET => Call::SystemReset,
        _ => Call::Return(NOT_SUPPORTED),
    }
}

#[cfg(test)]
mod tests {
    use hv::aarch64::mpidr_affinity;

    use super::*;

    fn call(args: [u64; 4]) -> Call {
        let mut regs = Regs::default();
        regs.x[..4].copy_from_slice(&args);
        decode(&regs)
    }

    #[test]
    fn version_and_migrate_are_answered_directly() {
        assert_eq!(call([FN_VERSION, 0, 0, 0]), Call::Return(VERSION_0_2));
        assert_eq!(
            call([FN_MIGRATE_INFO_TYPE, 0, 0, 0]),
            Call::Return(MIGRATE_NOT_REQUIRED)
        );
    }

    #[test]
    fn cpu_on_carries_entry_and_context() {
        let target = mpidr_affinity(17);
        assert_eq!(
            call([FN_CPU_ON, target, 0x8000_1000, 0xc0ffee]),
            Call::CpuOn {
                cpu: 17,
                entry: 0x8000_1000,
                context: 0xc0ffee
            }
        );
    }

    #[test]
    fn cpu_on_for_an_affinity_no_vcpu_has_is_invalid() {
        assert_eq!(
            call([FN_CPU_ON, 0xff, 0, 0]),
            Call::Return(INVALID_PARAMETERS)
        );
        assert_eq!(
            call([FN_AFFINITY_INFO, 1 << 16, 0, 0]),
            Call::Return(INVALID_PARAMETERS)
        );
    }

    #[test]
    fn power_calls() {
        assert_eq!(call([FN_SYSTEM_OFF, 0, 0, 0]), Call::SystemOff);
        assert_eq!(call([FN_SYSTEM_RESET, 0, 0, 0]), Call::SystemReset);
        assert_eq!(call([FN_CPU_OFF, 0, 0, 0]), Call::CpuOff);
    }

    #[test]
    fn upper_half_of_x0_is_ignored_and_unknown_calls_are_unsupported() {
        assert_eq!(
            call([0xdead_0000_0000_0000 | FN_SYSTEM_OFF, 0, 0, 0]),
            Call::SystemOff
        );
        assert_eq!(call([0x8400_00ff, 0, 0, 0]), Call::Return(NOT_SUPPORTED));
    }
}
