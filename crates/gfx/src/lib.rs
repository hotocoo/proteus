//! The host side of GPU acceleration: gfxstream's renderer, reached through `rutabaga_gfx`, as a
//! `devices::virtio::gpu3d::Renderer`.
//!
//! Built only with the `gfxstream` feature, which needs `libgfxstream_backend` at link and run time.
//! The renderer is started once per boot of the guest. Starting it with no working Vulkan driver
//! never returns (see `docs/gfxstream.md`), so whether a driver works is found out first, in a
//! separate process with a time limit: `proteus --gpu-check` calls `check` and exits.

#[cfg(feature = "gfxstream")]
mod gfxstream;

#[cfg(feature = "gfxstream")]
pub use gfxstream::{CAPSETS, Gfxstream, check};

/// Whether this build can accelerate graphics at all.
pub const AVAILABLE: bool = cfg!(feature = "gfxstream");
