//! Userspace networking for the guest: a NAT built on a TCP/IP stack, with DHCP, DNS, an `adb`
//! forward and a sinkhole for traffic the policy rejects. It needs no host driver or privilege.

pub mod config;
mod copy;
mod dhcp;
pub mod dns;
mod frames;
mod nat;
pub mod policy;
pub mod sni;

pub use nat::{HostResolver, Network, Resolver};
