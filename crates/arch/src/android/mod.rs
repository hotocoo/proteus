//! Android's boot artifacts: where the ramdisks sit inside `init_boot.img` and `vendor_boot.img`, and a
//! writer for the small ramdisk the VMM layers on top of them.

pub mod bootimg;
pub mod cpio;
pub mod lz4;
