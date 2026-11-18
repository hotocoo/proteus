//! Every message this crate can show. Code refers to these by name.

use std::fmt::Display;

pub fn worker_start_failed(err: &dyn Display) -> String {
    format!("network could not start: {err}")
}

pub fn adb_listening(port: u16) -> String {
    format!("adb: connect with `adb connect 127.0.0.1:{port}`")
}

pub fn adb_port_moved(wanted: u16, got: u16) -> String {
    format!("adb: port {wanted} is taken, listening on 127.0.0.1:{got} instead")
}

pub fn adb_unavailable(wanted: u16) -> String {
    format!("adb: no free loopback port at or just above {wanted}; adb forwarding is off")
}

pub fn sinkholed(what: &str, target: &dyn Display) -> String {
    format!("blocked {what} {target}")
}

pub const BLOCKED_DNS: &str = "lookup of";
pub const BLOCKED_TLS: &str = "TLS connection to";
pub const BLOCKED_CONNECT: &str = "connection to";

pub fn host_connect_failed(target: &dyn Display, err: &dyn Display) -> String {
    format!("could not reach {target}: {err}")
}

pub fn poll_failed(err: &dyn Display) -> String {
    format!("network event loop: {err}")
}

pub const WORKER_THREAD: &str = "net";
pub const RESOLVER_THREAD: &str = "net-dns";
pub const WORKER_PANICKED: &str = "network worker thread panicked";
pub const ROUTE_TABLE_FULL: &str = "route table is full";
