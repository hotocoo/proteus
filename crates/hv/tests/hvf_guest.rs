//! Runs tiny bare-metal guests on Hypervisor.framework to pin down what the backend must get right:
//! MMIO decode for every access width, sign extension, the zero register, HVC return, and kick.
//!
//! The guests are assembled into this test binary (host and guest are both ARM64) and copied into guest RAM.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

mod common;

use std::sync::{Condvar, Mutex};

use common::{GPA_BITS, GuestPage, MMIO_BASE, PSCI_SYSTEM_OFF, RAM_BASE};

use hv::aarch64::PSTATE_EL1H_DAIF_MASKED;
use hv::{Exit, Gpa, Host, HypervisorBackend, IoHandler, Kick, Prot, Regs, Vcpu, Vm, VmConfig};

/// The framework allows one VM per process, and the test harness runs tests on parallel threads.
static ONE_VM: Mutex<()> = Mutex::new(());

// Offsets the guest reads from; the host answers with the patterns below.
const READ_BYTE: u64 = 0x20;
const READ_HALF: u64 = 0x22;
const READ_WORD: u64 = 0x24;
const READ_QUAD: u64 = 0x28;
const BYTE_PATTERN: u8 = 0x80;
const HALF_PATTERN: u16 = 0x8001;
const WORD_PATTERN: u32 = 0x8000_0001;
const QUAD_PATTERN: u64 = 0x1122_3344_5566_7788;

// Offsets the guest reports results to.
const REPORT_BASE: u64 = 0x40;
const REPORT_AFTER_HVC: u64 = 0x78;

const HVC_RESULT: u64 = 0x1234;

core::arch::global_asm!(
    ".p2align 2",
    ".globl _guest_mmio_start",
    "_guest_mmio_start:",
    // Stores of every width, and one from the zero register.
    "mov  w1, #0x41",
    "strb w1, [x28]",
    "mov  w2, #0x4243",
    "strh w2, [x28, #2]",
    "movz w3, #0x4647",
    "movk w3, #0x4445, lsl #16",
    "str  w3, [x28, #4]",
    "movz x4, #0x7788",
    "movk x4, #0x5566, lsl #16",
    "movk x4, #0x3344, lsl #32",
    "movk x4, #0x1122, lsl #48",
    "str  x4, [x28, #8]",
    "str  xzr, [x28, #16]",
    // Loads into registers other than x0, zero- and sign-extended.
    "ldrb  w9,  [x28, #0x20]",
    "ldrsb w10, [x28, #0x20]",
    "ldrsb x11, [x28, #0x20]",
    "ldrsh x12, [x28, #0x22]",
    "ldrsw x13, [x28, #0x24]",
    "ldr   x14, [x28, #0x28]",
    "ldr   w15, [x28, #0x24]",
    // A load into the zero register must leave everything else alone.
    "ldr   xzr, [x28, #0x28]",
    // Report what landed in each register.
    "str x9,  [x28, #0x40]",
    "str x10, [x28, #0x48]",
    "str x11, [x28, #0x50]",
    "str x12, [x28, #0x58]",
    "str x13, [x28, #0x60]",
    "str x14, [x28, #0x68]",
    "str x15, [x28, #0x70]",
    // Hypercall, then report x0 so the host can see execution resumed after the HVC.
    "movz x0, #0x0008",
    "movk x0, #0x8400, lsl #16",
    "hvc  #0",
    "str  x0, [x28, #0x78]",
    // Spin until kicked.
    "b .",
    ".globl _guest_mmio_end",
    "_guest_mmio_end:",
);

unsafe extern "C" {
    static guest_mmio_start: u8;
    static guest_mmio_end: u8;
}

fn guest_code() -> &'static [u8] {
    // SAFETY: both symbols bound the assembly block above.
    unsafe { common::blob(&raw const guest_mmio_start, &raw const guest_mmio_end) }
}

#[derive(Default)]
struct Recorder {
    writes: Mutex<Vec<(u64, Vec<u8>)>>,
    wrote: Condvar,
}

impl Recorder {
    fn writes(&self) -> Vec<(u64, Vec<u8>)> {
        self.writes.lock().unwrap().clone()
    }

    fn wait_for_write_at(&self, offset: u64) {
        let seen = |writes: &mut Vec<(u64, Vec<u8>)>| !writes.iter().any(|(at, _)| *at == offset);
        drop(
            self.wrote
                .wait_while(self.writes.lock().unwrap(), seen)
                .unwrap(),
        );
    }
}

impl IoHandler for Recorder {
    fn mmio_read(&self, gpa: Gpa, data: &mut [u8]) {
        let pattern = match gpa - MMIO_BASE {
            READ_BYTE => u64::from(BYTE_PATTERN),
            READ_HALF => u64::from(HALF_PATTERN),
            READ_WORD => u64::from(WORD_PATTERN),
            READ_QUAD => QUAD_PATTERN,
            other => panic!("guest read unexpected offset {other:#x}"),
        };
        data.copy_from_slice(&pattern.to_le_bytes()[..data.len()]);
    }

    fn mmio_write(&self, gpa: Gpa, data: &[u8]) {
        self.writes
            .lock()
            .unwrap()
            .push((gpa - MMIO_BASE, data.to_vec()));
        self.wrote.notify_all();
    }
}

fn quad(value: u64) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

#[test]
fn mmio_widths_sign_extension_hvc_and_kick() {
    common::within_deadline(mmio_scenario);
}

fn mmio_scenario() {
    let _one_vm = ONE_VM.lock().unwrap();
    let caps = Host::probe().unwrap();
    let vm = Host::create_vm(&VmConfig { gpa_bits: GPA_BITS }).unwrap();
    let page = GuestPage::new(caps.host_page_size);
    page.load(guest_code());
    // SAFETY: `page` outlives the VM (declared before `vcpu`, dropped after `vm` goes out of use below).
    unsafe { vm.map(RAM_BASE, page.ptr, caps.host_page_size, Prot::RWX) }.unwrap();

    let mut vcpu = vm.create_vcpu(0).unwrap();
    let mut regs = Regs {
        pc: RAM_BASE,
        pstate: PSTATE_EL1H_DAIF_MASKED,
        ..Regs::default()
    };
    regs.x[28] = MMIO_BASE;
    vcpu.set_regs(&regs).unwrap();

    let io = Recorder::default();

    // First stop: the hypercall, with every MMIO access already served.
    assert_eq!(vcpu.run(&io).unwrap(), Exit::Hypercall);
    let mut regs = vcpu.regs().unwrap();
    assert_eq!(regs.x[0], PSCI_SYSTEM_OFF);
    assert_eq!(
        io.writes(),
        vec![
            (0, vec![0x41]),
            (2, vec![0x43, 0x42]),
            (4, vec![0x47, 0x46, 0x45, 0x44]),
            (8, quad(QUAD_PATTERN)),
            (16, quad(0)),
            (REPORT_BASE, quad(u64::from(BYTE_PATTERN))),
            (REPORT_BASE + 0x08, quad(0xffff_ff80)),
            (REPORT_BASE + 0x10, quad(0xffff_ffff_ffff_ff80)),
            (REPORT_BASE + 0x18, quad(0xffff_ffff_ffff_8001)),
            (REPORT_BASE + 0x20, quad(0xffff_ffff_8000_0001)),
            (REPORT_BASE + 0x28, quad(QUAD_PATTERN)),
            (REPORT_BASE + 0x30, quad(u64::from(WORD_PATTERN))),
        ]
    );

    // Second stop: the guest resumes after the HVC, reports x0, spins, and is kicked out.
    regs.x[0] = HVC_RESULT;
    vcpu.set_regs(&regs).unwrap();
    let kick = vcpu.kicker();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            io.wait_for_write_at(REPORT_AFTER_HVC);
            kick.kick();
        });
        assert_eq!(vcpu.run(&io).unwrap(), Exit::Kicked);
    });
    assert_eq!(
        io.writes().last().unwrap(),
        &(REPORT_AFTER_HVC, quad(HVC_RESULT))
    );

    drop(vcpu);
    drop(vm);
}

#[test]
fn second_vm_in_one_process_is_refused() {
    let _one_vm = ONE_VM.lock().unwrap();
    let vm = Host::create_vm(&VmConfig { gpa_bits: GPA_BITS }).unwrap();
    assert!(matches!(
        Host::create_vm(&VmConfig { gpa_bits: GPA_BITS }),
        Err(hv::HvError::Busy)
    ));
    drop(vm);
    // Once the first is gone a new one may be created.
    drop(Host::create_vm(&VmConfig { gpa_bits: GPA_BITS }).unwrap());
}

#[test]
fn unaligned_mapping_is_refused() {
    let _one_vm = ONE_VM.lock().unwrap();
    let caps = Host::probe().unwrap();
    let vm = Host::create_vm(&VmConfig { gpa_bits: GPA_BITS }).unwrap();
    let page = GuestPage::new(caps.host_page_size);
    // SAFETY: the page is valid; the call is expected to fail before mapping anything.
    let result = unsafe { vm.map(RAM_BASE + 0x1000, page.ptr, caps.host_page_size, Prot::RW) };
    assert!(matches!(result, Err(hv::HvError::BadArgument(_))));
}
