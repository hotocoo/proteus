//! Interrupt path on Hypervisor.framework's in-kernel GICv3: a virtual-timer interrupt wakes a vCPU
//! from WFI, a wired SPI raised from another thread does the same, PSCI CPU_ON starts a second vCPU,
//! and an SGI sent by one vCPU wakes the other from WFI with no help from the host.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

mod common;

use std::sync::{Condvar, Mutex};
use std::time::Duration;

use common::{GPA_BITS, GuestPage, MMIO_BASE, PSCI_SYSTEM_OFF, RAM_BASE};
use hv::aarch64::PSTATE_EL1H_DAIF_MASKED;
use hv::{
    Exit, Gpa, Host, HypervisorBackend, IoHandler, Irqchip, IrqchipConfig, Kick, Prot, Regs, Vcpu,
    Vm, VmConfig,
};

const GICD_BASE: Gpa = 0x0800_0000;
const GICR_BASE: Gpa = 0x080a_0000;
/// Any access in here reaching the VMM means the framework is not emulating its own GIC.
const GIC_WINDOW_END: Gpa = 0x0900_0000;

/// SPI line raised by the host, and the interrupt ID the guest must then acknowledge.
const SPI_LINE: u32 = 5;
const VTIMER_INTID: u64 = 27;
/// Guest timer delay in counter ticks: 1 ms at the 24 MHz the virtual counter runs at.
const TIMER_TICKS: u64 = 24_000;

const PSCI_CPU_ON: u64 = 0xc400_0003;
const PSCI_SUCCESS: u64 = 0;
const SECONDARY_MPIDR: u64 = 1;

// Offsets the guest reports to.
const REPORT_INTID: u64 = 0x08;
const REPORT_CPU_ON_RESULT: u64 = 0x10;
const REPORT_SECONDARY: u64 = 0x18;
/// The first CPU reads this before sending its SGI; the host answers once the second CPU sleeps.
const READ_SECONDARY_ASLEEP: u64 = 0x30;
/// SGI the first CPU sends to the second.
const SGI_INTID: u64 = 3;
/// How long the second CPU gets to go from "ready" to parked in WFI before the SGI is sent.
const SETTLE: Duration = Duration::from_millis(100);
const REPORT_UNEXPECTED_EXCEPTION: u64 = 0xf0;

core::arch::global_asm!(
    ".p2align 11",
    ".globl _guest_gic_start",
    "_guest_gic_start:",
    "adr  x0, 90f",
    "msr  vbar_el1, x0",
    // CPU interface: accept every priority, enable group 1.
    "mov  x0, #0xff",
    "msr  S3_0_C4_C6_0, x0", // ICC_PMR_EL1
    "mov  x0, #1",
    "msr  S3_0_C12_C12_7, x0", // ICC_IGRPEN1_EL1
    "isb",
    // Distributor: affinity routing and group 1.
    "mov  w0, #0x12",
    "str  w0, [x27]",
    // Redistributor: leave sleep, wait for the children to wake.
    "ldr  w0, [x26, #0x14]",
    "bic  w0, w0, #2",
    "str  w0, [x26, #0x14]",
    "1:",
    "ldr  w0, [x26, #0x14]",
    "tbnz w0, #2, 1b",
    // SGI/PPI frame: everything group 1, virtual timer PPI enabled.
    "add  x1, x26, #0x10, lsl #12",
    "mov  w0, #-1",
    "str  w0, [x1, #0x80]",
    "movz w0, #0x0800, lsl #16",
    "str  w0, [x1, #0x100]",
    // SPIs 32..63: group 1, enabled, the one under test routed to CPU 0.
    "mov  w0, #-1",
    "str  w0, [x27, #0x84]",
    "str  w0, [x27, #0x104]",
    "add  x1, x27, #0x6, lsl #12",
    "str  xzr, [x1, x25, lsl #3]",
    // Arm the virtual timer and unmask IRQs.
    "msr  cntv_tval_el0, x24",
    "mov  x0, #1",
    "msr  cntv_ctl_el0, x0",
    "msr  daifclr, #2",
    // Sleep until the handler has counted two interrupts: the timer, then the SPI.
    "2:",
    "wfi",
    "cmp  x20, #2",
    "b.lo 2b",
    // Leave the redistributor and MMIO bases where the second CPU can find them, then start it.
    "adr  x4, 85f",
    "str  x26, [x4]",
    "str  x28, [x4, #8]",
    "movz x0, #0x0003",
    "movk x0, #0xc400, lsl #16",
    "mov  x1, #1",
    "adr  x2, 80f",
    "mov  x3, x28",
    "hvc  #0",
    "str  x0, [x28, #0x10]",
    // The host holds this read until the second CPU is asleep in WFI.
    "ldr  x0, [x28, #0x30]",
    // SGI 3 to the CPU with Aff0 = 1: INTID in bits 27:24, target list bit 1.
    "movz x0, #0x0300, lsl #16",
    "orr  x0, x0, #2",
    "msr  S3_0_C12_C11_5, x0", // ICC_SGI1R_EL1
    "3:",
    "wfi",
    "b    3b",
    // IRQ handler: report the interrupt ID, quiesce the timer if it was the source, EOI.
    "70:",
    "mrs  x0, S3_0_C12_C12_0", // ICC_IAR1_EL1
    "str  x0, [x28, #0x08]",
    "cmp  x0, #27",
    "b.ne 71f",
    "msr  cntv_ctl_el0, xzr",
    "71:",
    "msr  S3_0_C12_C12_1, x0", // ICC_EOIR1_EL1
    "add  x20, x20, #1",
    "eret",
    // Second CPU: bring up its own interrupt path with no timer armed, report the context it was
    // started with, sleep until one interrupt has been handled, then power off.
    "80:",
    "mov  x19, x0",
    "adr  x4, 85f",
    "ldr  x26, [x4]",
    "ldr  x28, [x4, #8]",
    "add  x26, x26, #0x20, lsl #12", // this CPU's redistributor is the second frame pair
    "adr  x0, 90f",
    "msr  vbar_el1, x0",
    "mov  x0, #0xff",
    "msr  S3_0_C4_C6_0, x0",
    "mov  x0, #1",
    "msr  S3_0_C12_C12_7, x0",
    "isb",
    "ldr  w0, [x26, #0x14]",
    "bic  w0, w0, #2",
    "str  w0, [x26, #0x14]",
    "81:",
    "ldr  w0, [x26, #0x14]",
    "tbnz w0, #2, 81b",
    "add  x1, x26, #0x10, lsl #12",
    "mov  w0, #-1",
    "str  w0, [x1, #0x80]",
    "str  w0, [x1, #0x100]",
    "str  x19, [x28, #0x18]",
    "msr  daifclr, #2",
    "82:",
    "wfi",
    "cmp  x20, #1",
    "b.lo 82b",
    "movz x0, #0x0008",
    "movk x0, #0x8400, lsl #16",
    "hvc  #0",
    "b    .",
    // Bases handed from the first CPU to the second.
    ".p2align 3",
    "85:",
    ".quad 0",
    ".quad 0",
    // Vector table. Only "IRQ, current EL, SPx" (entry 5) is expected.
    ".p2align 11",
    "90:",
    ".rept 5",
    "mrs  x0, esr_el1",
    "str  x0, [x28, #0xf0]",
    "b    .",
    ".p2align 7",
    ".endr",
    "b    70b",
    ".p2align 7",
    ".rept 10",
    "mrs  x0, esr_el1",
    "str  x0, [x28, #0xf0]",
    "b    .",
    ".p2align 7",
    ".endr",
    ".globl _guest_gic_end",
    "_guest_gic_end:",
);

unsafe extern "C" {
    static guest_gic_start: u8;
    static guest_gic_end: u8;
}

struct Board<'a, I: Irqchip> {
    irqchip: &'a I,
    reports: Mutex<Vec<(u64, u64)>>,
    reported: Condvar,
}

impl<I: Irqchip> Board<'_, I> {
    fn wait_for(&self, offset: u64, value: u64) {
        let missing = |reports: &mut Vec<(u64, u64)>| !reports.contains(&(offset, value));
        drop(
            self.reported
                .wait_while(self.reports.lock().unwrap(), missing)
                .unwrap(),
        );
    }
}

impl<I: Irqchip> IoHandler for Board<'_, I> {
    fn mmio_read(&self, gpa: Gpa, data: &mut [u8]) {
        assert_eq!(
            gpa - MMIO_BASE,
            READ_SECONDARY_ASLEEP,
            "guest read {gpa:#x} reached the VMM"
        );
        self.wait_for(REPORT_SECONDARY, MMIO_BASE);
        // Nothing reports "now asleep"; the second CPU only has a few instructions left before its WFI.
        std::thread::sleep(SETTLE);
        data.fill(0);
    }

    fn mmio_write(&self, gpa: Gpa, data: &[u8]) {
        assert!(
            !(GICD_BASE..GIC_WINDOW_END).contains(&gpa),
            "GIC access {gpa:#x} reached the VMM"
        );
        let offset = gpa - MMIO_BASE;
        let value = u64::from_le_bytes(data.try_into().unwrap());
        assert_ne!(
            offset, REPORT_UNEXPECTED_EXCEPTION,
            "guest took an unexpected exception, ESR {value:#x}"
        );
        // A level-triggered line must drop before the guest signals end of interrupt.
        if (offset, value) == (REPORT_INTID, spi_intid()) {
            self.irqchip.set_irq(SPI_LINE, false).unwrap();
        }
        self.reports.lock().unwrap().push((offset, value));
        self.reported.notify_all();
    }
}

fn spi_intid() -> u64 {
    u64::from(hv::hvf::GicGeometry::query().unwrap().spi_base + SPI_LINE)
}

#[test]
fn timer_spi_and_sgi_each_wake_a_vcpu_from_wfi() {
    common::within_deadline(scenario);
}

fn scenario() {
    let caps = Host::probe().unwrap();
    let vm = Host::create_vm(&VmConfig { gpa_bits: GPA_BITS }).unwrap();
    let page = GuestPage::new(caps.host_page_size);
    // SAFETY: both symbols bound the assembly block above.
    page.load(unsafe { common::blob(&raw const guest_gic_start, &raw const guest_gic_end) });
    // SAFETY: `page` is dropped after the VM.
    unsafe { vm.map(RAM_BASE, page.ptr, caps.host_page_size, Prot::RWX) }.unwrap();

    let irqchip = vm
        .create_irqchip(&IrqchipConfig {
            gicd_base: GICD_BASE,
            gicr_base: GICR_BASE,
        })
        .unwrap();
    let board = Board {
        irqchip: &*irqchip,
        reports: Mutex::default(),
        reported: Condvar::new(),
    };

    let mut vcpu = vm.create_vcpu(0).unwrap();
    let mut regs = Regs {
        pc: RAM_BASE,
        pstate: PSTATE_EL1H_DAIF_MASKED,
        ..Regs::default()
    };
    regs.x[24] = TIMER_TICKS;
    regs.x[25] = spi_intid();
    regs.x[26] = GICR_BASE;
    regs.x[27] = GICD_BASE;
    regs.x[28] = MMIO_BASE;
    vcpu.set_regs(&regs).unwrap();
    let kick = vcpu.kicker();
    // The threads below take these by reference and everything else by value.
    let (vm, board, kick) = (&vm, &board, &kick);

    std::thread::scope(|scope| {
        // Raise the SPI from another thread once the timer interrupt has been seen, while vCPU 0 sleeps in WFI.
        scope.spawn(move || {
            board.wait_for(REPORT_INTID, VTIMER_INTID);
            board.irqchip.set_irq(SPI_LINE, true).unwrap();
        });

        assert_eq!(vcpu.run(board).unwrap(), Exit::Hypercall);
        let mut regs = vcpu.regs().unwrap();
        assert_eq!(
            (regs.x[0], regs.x[1], regs.x[3]),
            (PSCI_CPU_ON, SECONDARY_MPIDR, MMIO_BASE)
        );
        let (entry, context) = (regs.x[2], regs.x[3]);

        // What the board's PSCI handler will do: start a thread that owns the new vCPU.
        scope.spawn(move || {
            let mut secondary = vm.create_vcpu(1).unwrap();
            let mut regs = Regs {
                pc: entry,
                pstate: PSTATE_EL1H_DAIF_MASKED,
                ..Regs::default()
            };
            regs.x[0] = context;
            secondary.set_regs(&regs).unwrap();
            assert_eq!(secondary.run(board).unwrap(), Exit::Hypercall);
            assert_eq!(secondary.regs().unwrap().x[0], PSCI_SYSTEM_OFF);
        });

        // vCPU 0 gets its CPU_ON result, sends the SGI once vCPU 1 sleeps, then sleeps in WFI itself.
        // It is kicked only after vCPU 1 has reported the SGI, so no host-side wake-up helps vCPU 1.
        scope.spawn(move || {
            board.wait_for(REPORT_CPU_ON_RESULT, PSCI_SUCCESS);
            board.wait_for(REPORT_INTID, SGI_INTID);
            kick.kick();
        });
        regs.x[0] = PSCI_SUCCESS;
        vcpu.set_regs(&regs).unwrap();
        assert_eq!(vcpu.run(board).unwrap(), Exit::Kicked);
    });

    let reports = board.reports.lock().unwrap().clone();
    let intids: Vec<u64> = reports
        .iter()
        .filter(|(at, _)| *at == REPORT_INTID)
        .map(|(_, id)| *id)
        .collect();
    assert_eq!(intids, [VTIMER_INTID, spi_intid(), SGI_INTID]);

    drop(vcpu);
}
