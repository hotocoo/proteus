//! x86_64 pieces every backend shares: the register file the VMM reads and writes.

/// General-purpose registers, in the order the hardware numbers them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Regs {
    /// rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, r8 to r15.
    pub gpr: [u64; GPR_COUNT],
    pub rip: u64,
    pub rflags: u64,
}

pub const GPR_COUNT: usize = 16;
/// Guest physical address width the boards use: far more than their RAM and devices need.
pub const GPA_BITS: u8 = 39;
pub const RAX: usize = 0;
pub const RCX: usize = 1;
pub const RDX: usize = 2;
pub const RBX: usize = 3;
pub const RSP: usize = 4;
pub const RBP: usize = 5;
pub const RSI: usize = 6;
pub const RDI: usize = 7;

/// RFLAGS with only the bit that always reads as one.
pub const RFLAGS_RESERVED: u64 = 1 << 1;

/// A segment register as the hardware caches it: the descriptor's fields, unpacked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segment {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    /// Descriptor type: for code and data, accessed, writable/readable, expand-down/conforming and
    /// code bits.
    pub kind: u8,
    pub present: bool,
    pub dpl: u8,
    /// Default operation size is 32 bits.
    pub db: bool,
    /// A code or data segment, not a system one.
    pub s: bool,
    /// 64-bit code segment.
    pub l: bool,
    /// Limit counts 4 KiB pages.
    pub g: bool,
    /// Marks a segment register that holds nothing usable.
    pub unusable: bool,
}

/// A descriptor table register: GDTR or IDTR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub base: u64,
    pub limit: u16,
}

/// Control registers, segments and descriptor tables: what decides the mode the CPU runs in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemRegs {
    pub cs: Segment,
    pub ds: Segment,
    pub es: Segment,
    pub fs: Segment,
    pub gs: Segment,
    pub ss: Segment,
    pub tr: Segment,
    pub ldt: Segment,
    pub gdt: Table,
    pub idt: Table,
    pub cr0: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub efer: u64,
}

pub const CR0_PE: u64 = 1;
pub const CR0_ET: u64 = 1 << 4;
pub const CR0_PG: u64 = 1 << 31;
pub const CR4_PAE: u64 = 1 << 5;
pub const EFER_LME: u64 = 1 << 8;
pub const EFER_LMA: u64 = 1 << 10;

/// The APIC ID of a vCPU: its index. The MP table the board writes must say the same.
pub fn apic_id(index: u32) -> u32 {
    index
}
