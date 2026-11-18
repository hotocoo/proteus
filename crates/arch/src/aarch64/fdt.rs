//! Flattened device tree for the ARM64 board. It is the only hardware description the guest gets.

use std::ops::Range;

use hv::aarch64::mpidr_affinity;
use vm_fdt::FdtWriter;

use super::layout;
use crate::{ArchError, copy};

const PHANDLE_GIC: u32 = 1;
const PHANDLE_CLOCK: u32 = 2;

// Interrupt specifier cells of the `arm,gic-v3` binding.
const GIC_SPI: u32 = 0;
const GIC_PPI: u32 = 1;
const IRQ_TYPE_EDGE_RISING: u32 = 1;
const IRQ_TYPE_LEVEL_HIGH: u32 = 4;
const GIC_INTERRUPT_CELLS: u32 = 3;

/// Generic timer PPIs in binding order: secure physical, non-secure physical, virtual, hypervisor.
const TIMER_PPIS: [u32; 4] = [13, 14, 11, 10];

const ADDRESS_CELLS: u32 = 2;
const SIZE_CELLS: u32 = 2;
/// A CPU's `reg` is its MPIDR affinity in one cell, with no size.
const CPU_ADDRESS_CELLS: u32 = 1;
const CPU_SIZE_CELLS: u32 = 0;
/// A fixed clock takes no specifier cells.
const FIXED_CLOCK_CELLS: u32 = 0;

const COMPATIBLE: &str = "emu,virt";
const UART_NODE: &str = "uart";
const CLOCK_NAME: &str = "apb_pclk";

pub struct FdtConfig<'a> {
    pub ram_size: u64,
    pub vcpus: u32,
    pub cmdline: &'a str,
    /// Guest range of ramdisk plus bootconfig trailer.
    pub initrd: Option<Range<u64>>,
    /// Whether the debug UART exists.
    pub serial: bool,
    /// virtio-mmio slots that hold a device, in ascending order. Probe order follows node order.
    pub virtio_slots: &'a [u32],
    pub rng_seed: &'a [u8],
    pub kaslr_seed: u64,
}

fn node_name(name: &str, base: u64) -> String {
    format!("{name}@{base:x}")
}

pub fn build(cfg: &FdtConfig<'_>) -> Result<Vec<u8>, ArchError> {
    if cfg.vcpus == 0 || cfg.vcpus > layout::MAX_VCPUS {
        return Err(ArchError::Layout(copy::TOO_MANY_VCPUS));
    }
    if cfg
        .virtio_slots
        .iter()
        .any(|slot| *slot >= layout::VIRTIO_MMIO_SLOTS)
    {
        return Err(ArchError::Layout(copy::TOO_MANY_VIRTIO));
    }

    let mut fdt = FdtWriter::new()?;
    let root = fdt.begin_node("")?;
    fdt.property_string("compatible", COMPATIBLE)?;
    fdt.property_u32("#address-cells", ADDRESS_CELLS)?;
    fdt.property_u32("#size-cells", SIZE_CELLS)?;
    fdt.property_u32("interrupt-parent", PHANDLE_GIC)?;

    let chosen = fdt.begin_node("chosen")?;
    fdt.property_string("bootargs", cfg.cmdline)?;
    if cfg.serial {
        fdt.property_string(
            "stdout-path",
            &format!("/{}", node_name(UART_NODE, layout::UART_BASE)),
        )?;
    }
    if let Some(initrd) = &cfg.initrd {
        fdt.property_u64("linux,initrd-start", initrd.start)?;
        fdt.property_u64("linux,initrd-end", initrd.end)?;
    }
    fdt.property("rng-seed", cfg.rng_seed)?;
    fdt.property_u64("kaslr-seed", cfg.kaslr_seed)?;
    fdt.end_node(chosen)?;

    let memory = fdt.begin_node(&node_name("memory", layout::RAM_BASE))?;
    fdt.property_string("device_type", "memory")?;
    fdt.property_array_u64("reg", &[layout::RAM_BASE, cfg.ram_size])?;
    fdt.end_node(memory)?;

    let cpus = fdt.begin_node("cpus")?;
    fdt.property_u32("#address-cells", CPU_ADDRESS_CELLS)?;
    fdt.property_u32("#size-cells", CPU_SIZE_CELLS)?;
    for index in 0..cfg.vcpus {
        let affinity = mpidr_affinity(index);
        let cpu = fdt.begin_node(&node_name("cpu", affinity))?;
        fdt.property_string("device_type", "cpu")?;
        fdt.property_string("compatible", "arm,armv8")?;
        fdt.property_string("enable-method", "psci")?;
        fdt.property_u32(
            "reg",
            u32::try_from(affinity).expect("affinity of a valid vCPU fits one cell"),
        )?;
        fdt.end_node(cpu)?;
    }
    fdt.end_node(cpus)?;

    let psci = fdt.begin_node("psci")?;
    fdt.property_string("compatible", "arm,psci-0.2")?;
    fdt.property_string("method", "hvc")?;
    fdt.end_node(psci)?;

    let gic = fdt.begin_node(&node_name("intc", layout::GICD_BASE))?;
    fdt.property_string("compatible", "arm,gic-v3")?;
    fdt.property_null("interrupt-controller")?;
    fdt.property_u32("#interrupt-cells", GIC_INTERRUPT_CELLS)?;
    fdt.property_array_u64(
        "reg",
        &[
            layout::GICD_BASE,
            layout::GICD_SIZE,
            layout::GICR_BASE,
            u64::from(cfg.vcpus) * layout::GICR_SIZE_PER_VCPU,
        ],
    )?;
    fdt.property_phandle(PHANDLE_GIC)?;
    fdt.end_node(gic)?;

    let timer = fdt.begin_node("timer")?;
    fdt.property_string("compatible", "arm,armv8-timer")?;
    fdt.property_null("always-on")?;
    let timer_irqs: Vec<u32> = TIMER_PPIS
        .iter()
        .flat_map(|ppi| [GIC_PPI, *ppi, IRQ_TYPE_LEVEL_HIGH])
        .collect();
    fdt.property_array_u32("interrupts", &timer_irqs)?;
    fdt.end_node(timer)?;

    let clock = fdt.begin_node("apb-pclk")?;
    fdt.property_string("compatible", "fixed-clock")?;
    fdt.property_u32("#clock-cells", FIXED_CLOCK_CELLS)?;
    fdt.property_u32("clock-frequency", layout::APB_CLOCK_HZ)?;
    fdt.property_string("clock-output-names", CLOCK_NAME)?;
    fdt.property_phandle(PHANDLE_CLOCK)?;
    fdt.end_node(clock)?;

    if cfg.serial {
        // vm-superio's 16550 only pulses its interrupt, so the line is described as edge-triggered.
        let uart = fdt.begin_node(&node_name(UART_NODE, layout::UART_BASE))?;
        fdt.property_string("compatible", "ns16550a")?;
        fdt.property_array_u64("reg", &[layout::UART_BASE, layout::UART_SIZE])?;
        fdt.property_u32("clock-frequency", layout::UART_CLOCK_HZ)?;
        fdt.property_array_u32(
            "interrupts",
            &[GIC_SPI, layout::UART_SPI, IRQ_TYPE_EDGE_RISING],
        )?;
        fdt.end_node(uart)?;
    }

    let rtc = fdt.begin_node(&node_name("rtc", layout::RTC_BASE))?;
    fdt.property_string_list(
        "compatible",
        vec!["arm,pl031".into(), "arm,primecell".into()],
    )?;
    fdt.property_array_u64("reg", &[layout::RTC_BASE, layout::RTC_SIZE])?;
    fdt.property_array_u32(
        "interrupts",
        &[GIC_SPI, layout::RTC_SPI, IRQ_TYPE_LEVEL_HIGH],
    )?;
    fdt.property_u32("clocks", PHANDLE_CLOCK)?;
    fdt.property_string("clock-names", CLOCK_NAME)?;
    fdt.end_node(rtc)?;

    for slot in cfg.virtio_slots {
        let base = layout::virtio_mmio_base(*slot);
        let node = fdt.begin_node(&node_name("virtio_mmio", base))?;
        fdt.property_string("compatible", "virtio,mmio")?;
        fdt.property_array_u64("reg", &[base, layout::VIRTIO_MMIO_STRIDE])?;
        fdt.property_array_u32(
            "interrupts",
            &[GIC_SPI, layout::virtio_spi(*slot), IRQ_TYPE_LEVEL_HIGH],
        )?;
        fdt.property_null("dma-coherent")?;
        fdt.end_node(node)?;
    }

    fdt.end_node(root)?;
    Ok(fdt.finish()?)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::*;

    const GOLDEN: &str = "tests/golden/aarch64_board.dts";

    fn sample() -> Vec<u8> {
        build(&FdtConfig {
            ram_size: 1 << 30,
            vcpus: 2,
            cmdline: "console=ttyS0",
            initrd: Some(0x8800_0000..0x8810_0000),
            serial: true,
            virtio_slots: &[0, 1, 2],
            rng_seed: &[0xab; 8],
            kaslr_seed: 0x1122_3344_5566_7788,
        })
        .unwrap()
    }

    /// Decompiles with `dtc` and compares with the checked-in source.
    /// Set `BLESS_GOLDEN=1` to rewrite it after an intended change.
    #[test]
    fn board_matches_golden_device_tree() {
        let blob = std::env::temp_dir().join(format!("emu-fdt-{}.dtb", std::process::id()));
        std::fs::write(&blob, sample()).unwrap();
        let output = Command::new("dtc")
            .args(["-I", "dtb", "-O", "dts", "-q"])
            .arg(&blob)
            .output();
        let _ = std::fs::remove_file(&blob);
        let Ok(output) = output else {
            eprintln!("dtc is not installed; device tree golden test skipped");
            return;
        };
        assert!(
            output.status.success(),
            "dtc rejected the blob: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let dts = String::from_utf8(output.stdout).unwrap();
        let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
        if std::env::var_os("BLESS_GOLDEN").is_some() {
            std::fs::write(&golden, &dts).unwrap();
        }
        assert_eq!(dts, std::fs::read_to_string(&golden).unwrap());
    }

    #[test]
    fn nodes_without_a_device_are_left_out() {
        let with = sample();
        let without = build(&FdtConfig {
            ram_size: 1 << 30,
            vcpus: 1,
            cmdline: "",
            initrd: None,
            serial: false,
            virtio_slots: &[],
            rng_seed: &[],
            kaslr_seed: 0,
        })
        .unwrap();
        let has =
            |blob: &[u8], needle: &[u8]| blob.windows(needle.len()).any(|window| window == needle);
        assert!(
            has(&with, b"ns16550a")
                && has(&with, b"virtio,mmio")
                && has(&with, b"linux,initrd-start")
        );
        assert!(!has(&without, b"ns16550a") && !has(&without, b"virtio,mmio"));
        assert!(!has(&without, b"linux,initrd-start") && !has(&without, b"stdout-path"));
    }

    #[test]
    fn limits_are_enforced() {
        let cfg = |vcpus, slots: &'static [u32]| FdtConfig {
            ram_size: 1 << 30,
            vcpus,
            cmdline: "",
            initrd: None,
            serial: false,
            virtio_slots: slots,
            rng_seed: &[],
            kaslr_seed: 0,
        };
        assert!(build(&cfg(0, &[])).is_err());
        assert!(build(&cfg(layout::MAX_VCPUS + 1, &[])).is_err());
        assert!(build(&cfg(1, &[layout::VIRTIO_MMIO_SLOTS])).is_err());
        assert!(build(&cfg(layout::MAX_VCPUS, &[layout::VIRTIO_MMIO_SLOTS - 1])).is_ok());
    }
}
