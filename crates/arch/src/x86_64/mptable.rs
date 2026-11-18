//! The MP table (Intel MultiProcessor Specification 1.4): how a guest without ACPI finds its CPUs,
//! its IOAPIC and which interrupt line is wired to which IOAPIC pin.

use vm_memory::{Bytes, GuestAddress, GuestMemory};

use super::layout::{EBDA_START, IOAPIC_ADDR, IOAPIC_PINS, LAPIC_ADDR};
use crate::{ArchError, copy};

const FLOATING_SIGNATURE: &[u8; 4] = b"_MP_";
const TABLE_SIGNATURE: &[u8; 4] = b"PCMP";
const FLOATING_LEN: usize = 16;
const HEADER_LEN: usize = 44;
const SPEC_REVISION: u8 = 4;
const OEM_ID: &[u8; 8] = b"EMU     ";
const PRODUCT_ID: &[u8; 12] = b"EMU         ";
const BUS_ISA: &[u8; 6] = b"ISA   ";

const ENTRY_PROCESSOR: u8 = 0;
const ENTRY_BUS: u8 = 1;
const ENTRY_IOAPIC: u8 = 2;
const ENTRY_IO_INTERRUPT: u8 = 3;
const ENTRY_LOCAL_INTERRUPT: u8 = 4;

const LAPIC_VERSION: u8 = 0x14;
const IOAPIC_VERSION: u8 = 0x11;
const CPU_ENABLED: u8 = 1;
const CPU_BOOTSTRAP: u8 = 2;
const IOAPIC_ENABLED: u8 = 1;
/// Family 6 in the CPU signature; the features say a local APIC and an FPU are there.
const CPU_SIGNATURE: u32 = 0x600;
const CPU_FEATURES: u32 = 1 << 9 | 1;
const INTERRUPT_INT: u8 = 0;
const INTERRUPT_NMI: u8 = 1;
const INTERRUPT_EXTINT: u8 = 3;
const BUS_ID: u8 = 0;
/// Destination meaning every local APIC.
const ALL_LAPICS: u8 = 0xff;
/// The extended BIOS data area is one kilobyte; the table has to fit in it.
const EBDA_LEN: usize = 0x400;

fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)))
}

/// The floating pointer and the configuration table that follows it, for `cpus` CPUs whose local
/// APIC IDs are their indices.
pub fn build(cpus: u32) -> Result<Vec<u8>, ArchError> {
    let ioapic_id =
        u8::try_from(cpus).map_err(|_| ArchError::Layout(copy::MPTABLE_TOO_MANY_CPUS))?;
    let table_at = EBDA_START + FLOATING_LEN as u64;
    let mut entries = Vec::new();
    for cpu in 0..cpus {
        let flags = CPU_ENABLED | if cpu == 0 { CPU_BOOTSTRAP } else { 0 };
        entries.extend([ENTRY_PROCESSOR, cpu as u8, LAPIC_VERSION, flags]);
        entries.extend(CPU_SIGNATURE.to_le_bytes());
        entries.extend(CPU_FEATURES.to_le_bytes());
        entries.extend([0u8; 8]);
    }
    entries.extend([ENTRY_BUS, BUS_ID]);
    entries.extend(BUS_ISA);
    entries.extend([ENTRY_IOAPIC, ioapic_id, IOAPIC_VERSION, IOAPIC_ENABLED]);
    entries.extend((IOAPIC_ADDR as u32).to_le_bytes());
    for pin in 0..IOAPIC_PINS as u8 {
        entries.extend([
            ENTRY_IO_INTERRUPT,
            INTERRUPT_INT,
            0,
            0,
            BUS_ID,
            pin,
            ioapic_id,
            pin,
        ]);
    }
    entries.extend([
        ENTRY_LOCAL_INTERRUPT,
        INTERRUPT_EXTINT,
        0,
        0,
        BUS_ID,
        0,
        0,
        0,
    ]);
    entries.extend([
        ENTRY_LOCAL_INTERRUPT,
        INTERRUPT_NMI,
        0,
        0,
        BUS_ID,
        0,
        ALL_LAPICS,
        1,
    ]);
    let entry_count = cpus as u16 + 1 + 1 + IOAPIC_PINS as u16 + 2;

    let mut table = Vec::with_capacity(HEADER_LEN + entries.len());
    table.extend(TABLE_SIGNATURE);
    table.extend(((HEADER_LEN + entries.len()) as u16).to_le_bytes());
    table.extend([SPEC_REVISION, 0]);
    table.extend(OEM_ID);
    table.extend(PRODUCT_ID);
    table.extend([0u8; 4 + 2]);
    table.extend(entry_count.to_le_bytes());
    table.extend((LAPIC_ADDR as u32).to_le_bytes());
    table.extend([0u8; 4]);
    table.extend(entries);
    table[7] = checksum(&table);

    let mut floating = Vec::with_capacity(FLOATING_LEN);
    floating.extend(FLOATING_SIGNATURE);
    floating.extend((table_at as u32).to_le_bytes());
    floating.extend([1, SPEC_REVISION, 0, 0, 0, 0, 0, 0]);
    floating[10] = checksum(&floating);

    floating.extend(table);
    if floating.len() > EBDA_LEN {
        return Err(ArchError::Layout(copy::MPTABLE_TOO_MANY_CPUS));
    }
    Ok(floating)
}

/// Writes the MP table for `cpus` CPUs at the start of the extended BIOS data area.
pub fn write<M: GuestMemory>(mem: &M, cpus: u32) -> Result<(), ArchError> {
    mem.write_slice(&build(cpus)?, GuestAddress(EBDA_START))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(bytes: &[u8]) -> u8 {
        bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))
    }

    #[test]
    fn both_structures_check_out_and_point_at_each_other() {
        let bytes = build(4).unwrap();
        let (floating, table) = bytes.split_at(FLOATING_LEN);
        assert_eq!(&floating[..4], FLOATING_SIGNATURE);
        assert_eq!(sum(floating), 0);
        let pointer = u32::from_le_bytes(floating[4..8].try_into().unwrap());
        assert_eq!(u64::from(pointer), EBDA_START + FLOATING_LEN as u64);
        assert_eq!(&table[..4], TABLE_SIGNATURE);
        let length = u16::from_le_bytes([table[4], table[5]]) as usize;
        assert_eq!(length, table.len());
        assert_eq!(sum(table), 0);
        let count = u16::from_le_bytes([table[34], table[35]]);
        assert_eq!(count, 4 + 1 + 1 + 24 + 2);
        // The first processor is the bootstrap one, the others only enabled.
        assert_eq!(table[HEADER_LEN + 3], CPU_ENABLED | CPU_BOOTSTRAP);
        assert_eq!(table[HEADER_LEN + 20 + 3], CPU_ENABLED);
    }

    #[test]
    fn too_many_cpus_for_the_data_area_are_refused() {
        assert!(build(32).is_ok());
        assert!(build(64).is_err());
    }
}
