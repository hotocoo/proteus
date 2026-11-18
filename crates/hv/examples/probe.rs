//! Smallest possible check that this process may use the host hypervisor:
//! read its limits, create a VM, destroy it.

use hv::{Host, HypervisorBackend, VmConfig};

/// Address width the ARM64 board's map needs; kept here only because the board crate does not exist for a probe.
const PROBE_GPA_BITS: u8 = 36;

fn main() -> Result<(), hv::HvError> {
    let caps = Host::probe()?;
    println!("{caps:?}");
    let vm = Host::create_vm(&VmConfig {
        gpa_bits: PROBE_GPA_BITS,
    })?;
    drop(vm);
    println!("vm created and destroyed");
    Ok(())
}
