//! Runs tiny real-mode guests on KVM to pin down what the backend must get right: port I/O in both
//! directions, MMIO to unmapped memory, and kicks.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::sync::Mutex;
use std::time::Duration;

use common::{GuestPage, within_deadline};

use hv::x86_64::RFLAGS_RESERVED;
use hv::{
    Exit, Gpa, Host, HypervisorBackend, IoHandler, IrqchipConfig, Kick, Prot, Regs, Vcpu, Vm,
    VmConfig,
};

const RAM_LEN: usize = 0x8000;
const CODE_AT: u64 = 0x1000;
/// Inside the first megabyte, past the RAM: nothing is mapped there.
const MMIO_AT: Gpa = 0xf000;
const DATA_PORT: u16 = 0x3f8;
const ECHO_PORT: u16 = 0x3f9;
const DONE_PORT: u16 = 0x3fa;
const WRITTEN: u8 = 0x42;
const ANSWER: u8 = 0x5a;
const STORED: u8 = 0x99;

/// mov al, 0x42; mov dx, 0x3f8; out dx, al; in al, dx; mov dx, 0x3f9; out dx, al;
/// mov byte [0xf000], 0x99; mov dx, 0x3fa; out dx, al; jmp $
const IO_GUEST: [u8; 25] = [
    0xb0, WRITTEN, 0xba, 0xf8, 0x03, 0xee, 0xec, 0xba, 0xf9, 0x03, 0xee, 0xc6, 0x06, 0x00, 0xf0,
    STORED, 0xba, 0xfa, 0x03, 0xee, 0xeb, 0xfe, 0x90, 0x90, 0x90,
];
/// jmp $
const SPIN_GUEST: [u8; 2] = [0xeb, 0xfe];

#[derive(Debug, PartialEq, Eq)]
enum Seen {
    PortOut(u16, Vec<u8>),
    PortIn(u16),
    MmioOut(Gpa, Vec<u8>),
}

struct Recorder<K: Kick> {
    seen: Mutex<Vec<Seen>>,
    kick: K,
}

impl<K: Kick> IoHandler for Recorder<K> {
    fn mmio_read(&self, _gpa: Gpa, data: &mut [u8]) {
        data.fill(0);
    }

    fn mmio_write(&self, gpa: Gpa, data: &[u8]) {
        self.seen
            .lock()
            .unwrap()
            .push(Seen::MmioOut(gpa, data.to_vec()));
    }

    fn pio_read(&self, port: u16, data: &mut [u8]) {
        self.seen.lock().unwrap().push(Seen::PortIn(port));
        data.fill(ANSWER);
    }

    fn pio_write(&self, port: u16, data: &[u8]) {
        self.seen
            .lock()
            .unwrap()
            .push(Seen::PortOut(port, data.to_vec()));
        if port == DONE_PORT {
            self.kick.kick();
        }
    }
}

/// A VM with `code` at `CODE_AT`, and its one vCPU in real mode about to run it.
fn real_mode(
    code: &[u8],
) -> (
    <Host as HypervisorBackend>::Vm,
    <<Host as HypervisorBackend>::Vm as Vm>::Vcpu,
    GuestPage,
) {
    let vm = Host::create_vm(&VmConfig { gpa_bits: 32 }).unwrap();
    let ram = GuestPage::new(RAM_LEN);
    let mut image = vec![0u8; CODE_AT as usize];
    image.extend_from_slice(code);
    ram.load(&image);
    // SAFETY: `ram` outlives the VM in every test, and is page-aligned and page-sized.
    unsafe { vm.map(0, ram.ptr, RAM_LEN, Prot::RWX) }.unwrap();
    vm.create_irqchip(&IrqchipConfig {}).unwrap();
    let mut vcpu = vm.create_vcpu(0).unwrap();
    let mut system = vcpu.system_regs().unwrap();
    system.cs.base = 0;
    system.cs.selector = 0;
    vcpu.set_system_regs(&system).unwrap();
    vcpu.set_regs(&Regs {
        rip: CODE_AT,
        rflags: RFLAGS_RESERVED,
        ..Regs::default()
    })
    .unwrap();
    (vm, vcpu, ram)
}

#[test]
fn port_io_and_mmio_reach_the_handler_in_order() {
    within_deadline(|| {
        let (_vm, mut vcpu, _ram) = real_mode(&IO_GUEST);
        let recorder = Recorder {
            seen: Mutex::new(Vec::new()),
            kick: vcpu.kicker(),
        };
        assert_eq!(vcpu.run(&recorder).unwrap(), Exit::Kicked);
        assert_eq!(
            *recorder.seen.lock().unwrap(),
            [
                Seen::PortOut(DATA_PORT, vec![WRITTEN]),
                Seen::PortIn(DATA_PORT),
                Seen::PortOut(ECHO_PORT, vec![ANSWER]),
                Seen::MmioOut(MMIO_AT, vec![STORED]),
                Seen::PortOut(DONE_PORT, vec![ANSWER]),
            ]
        );
    });
}

#[test]
fn a_kick_from_another_thread_stops_a_spinning_guest_and_one_before_run_is_kept() {
    within_deadline(|| {
        let (_vm, mut vcpu, _ram) = real_mode(&SPIN_GUEST);
        let recorder = Recorder {
            seen: Mutex::new(Vec::new()),
            kick: vcpu.kicker(),
        };
        let kick = vcpu.kicker();
        let kicker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            kick.kick();
        });
        assert_eq!(vcpu.run(&recorder).unwrap(), Exit::Kicked);
        kicker.join().unwrap();
        // A kick that arrives while the vCPU is not running stops the next run at once.
        vcpu.kicker().kick();
        assert_eq!(vcpu.run(&recorder).unwrap(), Exit::Kicked);
        assert!(recorder.seen.lock().unwrap().is_empty());
    });
}
