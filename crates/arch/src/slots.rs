//! Which device sits in which virtio-mmio slot, the same on every board (blueprint section 4.4).
//! Disks come first so that the guest names them `vda`, `vdb`, `vdc`.

pub const VIRTIO_DISK_SLOTS: u32 = 3;
pub const VIRTIO_GPU_SLOT: u32 = 3;
pub const VIRTIO_TOUCH_SLOT: u32 = 4;
pub const VIRTIO_KEYBOARD_SLOT: u32 = 5;
pub const VIRTIO_GAMEPAD_SLOT: u32 = 6;
pub const VIRTIO_SOUND_SLOT: u32 = 7;
pub const VIRTIO_CONSOLE_SLOT: u32 = 9;
/// Network cards that exist only so that the real one is not the guest's first: the guest numbers
/// its cards in slot order, and an Android image keeps the first for its (absent) mobile network.
/// There are two because the image's optional Wi-Fi wrapper wants the real card to be the third.
pub const VIRTIO_SPARE_NET_SLOTS: [u32; 2] = [10, 11];
pub const VIRTIO_NET_SLOT: u32 = 12;
/// Last, so that adding it renumbers no other device.
pub const VIRTIO_BALLOON_SLOT: u32 = 13;

const _: () = {
    assert!(VIRTIO_DISK_SLOTS <= VIRTIO_GPU_SLOT && VIRTIO_GPU_SLOT < VIRTIO_TOUCH_SLOT);
    assert!(VIRTIO_TOUCH_SLOT < VIRTIO_KEYBOARD_SLOT && VIRTIO_KEYBOARD_SLOT < VIRTIO_GAMEPAD_SLOT);
    assert!(VIRTIO_GAMEPAD_SLOT < VIRTIO_SOUND_SLOT);
    assert!(VIRTIO_SOUND_SLOT < VIRTIO_CONSOLE_SLOT);
    assert!(VIRTIO_CONSOLE_SLOT < VIRTIO_SPARE_NET_SLOTS[0]);
    assert!(VIRTIO_SPARE_NET_SLOTS[0] < VIRTIO_SPARE_NET_SLOTS[1]);
    assert!(VIRTIO_SPARE_NET_SLOTS[1] < VIRTIO_NET_SLOT);
    assert!(VIRTIO_NET_SLOT < VIRTIO_BALLOON_SLOT);
};
