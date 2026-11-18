//! Which SPI trigger modes the framework's GICv3 lets a guest configure. The board's device tree may
//! only describe interrupts in a mode the guest kernel can actually program, because newer kernels
//! refuse to map an interrupt whose trigger configuration does not stick.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

mod common;

use std::sync::Mutex;

use common::{GPA_BITS, GuestPage, MMIO_BASE, RAM_BASE};
use hv::aarch64::PSTATE_EL1H_DAIF_MASKED;
use hv::{
    Exit, Gpa, Host, HypervisorBackend, IoHandler, IrqchipConfig, Prot, Regs, Vcpu, Vm, VmConfig,
};

const GICD_BASE: Gpa = 0x0800_0000;
const GICR_BASE: Gpa = 0x080a_0000;

/// GICD_ICFGR<n> holds two bits per interrupt; bit 1 of a pair set means edge-triggered.
/// Interrupt IDs 32..47 are in GICD_ICFGR2, at offset 0xc08.
const EDGE_ALL: u64 = 0xaaaa_aaaa;
const LEVEL_ALL: u64 = 0;

// Offsets the guest reports to.
const REPORT_INITIAL: u64 = 0x00;
const REPORT_AFTER_EDGE: u64 = 0x08;
const REPORT_AFTER_LEVEL: u64 = 0x10;

core::arch::global_asm!(
    ".p2align 2",
    ".globl _guest_icfgr_start",
    "_guest_icfgr_start:",
    // Affinity routing on, as Linux has it when it programs trigger modes.
    "mov  w0, #0x12",
    "str  w0, [x27]",
    "ldr  w0, [x27, #0xc08]",
    "str  x0, [x28, #0x00]",
    "movz w1, #0xaaaa",
    "movk w1, #0xaaaa, lsl #16",
    "str  w1, [x27, #0xc08]",
    "ldr  w0, [x27, #0xc08]",
    "str  x0, [x28, #0x08]",
    "str  wzr, [x27, #0xc08]",
    "ldr  w0, [x27, #0xc08]",
    "str  x0, [x28, #0x10]",
    "hvc  #0",
    ".globl _guest_icfgr_end",
    "_guest_icfgr_end:",
);

unsafe extern "C" {
    static guest_icfgr_start: u8;
    static guest_icfgr_end: u8;
}

#[derive(Default)]
struct Reports(Mutex<Vec<(u64, u64)>>);

impl IoHandler for Reports {
    fn mmio_read(&self, gpa: Gpa, _data: &mut [u8]) {
        panic!("guest read {gpa:#x} reached the VMM");
    }

    fn mmio_write(&self, gpa: Gpa, data: &[u8]) {
        let value = u64::from_le_bytes(data.try_into().unwrap());
        self.0.lock().unwrap().push((gpa - MMIO_BASE, value));
    }
}

#[test]
fn guest_can_switch_spis_between_level_and_edge() {
    common::within_deadline(|| {
        let caps = Host::probe().unwrap();
        let vm = Host::create_vm(&VmConfig { gpa_bits: GPA_BITS }).unwrap();
        let page = GuestPage::new(caps.host_page_size);
        // SAFETY: both symbols bound the assembly block above.
        page.load(unsafe {
            common::blob(&raw const guest_icfgr_start, &raw const guest_icfgr_end)
        });
        // SAFETY: `page` is dropped after the VM.
        unsafe { vm.map(RAM_BASE, page.ptr, caps.host_page_size, Prot::RWX) }.unwrap();
        let _irqchip = vm
            .create_irqchip(&IrqchipConfig {
                gicd_base: GICD_BASE,
                gicr_base: GICR_BASE,
            })
            .unwrap();
        let mut vcpu = vm.create_vcpu(0).unwrap();
        let mut regs = Regs {
            pc: RAM_BASE,
            pstate: PSTATE_EL1H_DAIF_MASKED,
            ..Regs::default()
        };
        regs.x[27] = GICD_BASE;
        regs.x[28] = MMIO_BASE;
        vcpu.set_regs(&regs).unwrap();

        let reports = Reports::default();
        assert_eq!(vcpu.run(&reports).unwrap(), Exit::Hypercall);
        let reports = reports.0.lock().unwrap().clone();
        eprintln!("GICD_ICFGR2 as the guest saw it: {reports:x?}");
        assert_eq!(
            reports[1],
            (REPORT_AFTER_EDGE, EDGE_ALL),
            "edge configuration did not stick"
        );
        assert_eq!(
            reports[2],
            (REPORT_AFTER_LEVEL, LEVEL_ALL),
            "level configuration did not stick"
        );
        assert_eq!(reports[0].0, REPORT_INITIAL);
        drop(vcpu);
    });
}
