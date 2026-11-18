//! Reading the frames the guest sends and writing the ones the NAT answers with.

use std::net::{Ipv4Addr, SocketAddrV4};

use devices::virtio::net::{MAC_LEN, MAX_FRAME_LEN};
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, IpAddress, IpProtocol,
    Ipv4Packet, Ipv4Repr, TcpPacket, UDP_HEADER_LEN, UdpPacket, UdpRepr,
};

/// Hop limit of packets the NAT originates.
const HOP_LIMIT: u8 = 64;
pub const BROADCAST_MAC: [u8; MAC_LEN] = [0xff; MAC_LEN];

#[derive(Debug, PartialEq, Eq)]
pub struct UdpDatagram<'a> {
    pub src: SocketAddrV4,
    pub dst: SocketAddrV4,
    pub payload: &'a [u8],
}

/// What to do with a frame the guest sent.
#[derive(Debug, PartialEq, Eq)]
pub enum Ingress<'a> {
    /// UDP is relayed or answered by the NAT itself; the TCP/IP stack never sees it.
    Udp(UdpDatagram<'a>),
    /// The first packet of a TCP connection. The stack needs a listening socket before it sees this.
    TcpSyn {
        src: SocketAddrV4,
        dst: SocketAddrV4,
    },
    /// ARP, ICMP and the rest of TCP go to the stack as they are.
    Stack,
    /// Not IPv4, fragmented, or malformed.
    Drop,
}

pub fn classify(bytes: &[u8]) -> Ingress<'_> {
    let Ok(frame) = EthernetFrame::new_checked(bytes) else {
        return Ingress::Drop;
    };
    match frame.ethertype() {
        EthernetProtocol::Arp => return Ingress::Stack,
        EthernetProtocol::Ipv4 => {}
        _ => return Ingress::Drop,
    }
    let Ok(packet) = Ipv4Packet::new_checked(frame.payload()) else {
        return Ingress::Drop;
    };
    // shortcut: fragmented datagrams are dropped, not reassembled; add reassembly if a game needs
    // UDP payloads above one frame.
    if packet.more_frags() || packet.frag_offset() != 0 {
        return Ingress::Drop;
    }
    let (src_ip, dst_ip) = (packet.src_addr(), packet.dst_addr());
    match packet.next_header() {
        IpProtocol::Udp => match UdpPacket::new_checked(packet.payload()) {
            Ok(udp) => {
                // Slice the payload out of the original frame so it outlives the parsers. A short
                // frame is padded past the datagram, so the end comes from the UDP length field.
                let start = EthernetFrame::<&[u8]>::header_len()
                    + usize::from(packet.header_len())
                    + UDP_HEADER_LEN;
                Ingress::Udp(UdpDatagram {
                    src: SocketAddrV4::new(src_ip, udp.src_port()),
                    dst: SocketAddrV4::new(dst_ip, udp.dst_port()),
                    payload: &bytes[start..start + udp.payload().len()],
                })
            }
            Err(_) => Ingress::Drop,
        },
        IpProtocol::Tcp => match TcpPacket::new_checked(packet.payload()) {
            Ok(tcp) if tcp.syn() && !tcp.ack() => Ingress::TcpSyn {
                src: SocketAddrV4::new(src_ip, tcp.src_port()),
                dst: SocketAddrV4::new(dst_ip, tcp.dst_port()),
            },
            Ok(_) => Ingress::Stack,
            Err(_) => Ingress::Drop,
        },
        IpProtocol::Icmp => Ingress::Stack,
        _ => Ingress::Drop,
    }
}

/// One Ethernet frame carrying a UDP datagram, or `None` if the payload does not fit in a frame.
pub fn udp_frame(
    src_mac: [u8; MAC_LEN],
    dst_mac: [u8; MAC_LEN],
    src: SocketAddrV4,
    dst: SocketAddrV4,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let udp = UdpRepr {
        src_port: src.port(),
        dst_port: dst.port(),
    };
    let ip = Ipv4Repr {
        src_addr: *src.ip(),
        dst_addr: *dst.ip(),
        next_header: IpProtocol::Udp,
        payload_len: udp.header_len() + payload.len(),
        hop_limit: HOP_LIMIT,
    };
    let ethernet = EthernetRepr {
        src_addr: EthernetAddress(src_mac),
        dst_addr: EthernetAddress(dst_mac),
        ethertype: EthernetProtocol::Ipv4,
    };
    let len = ethernet.buffer_len() + ip.buffer_len() + ip.payload_len;
    if len > MAX_FRAME_LEN {
        return None;
    }
    let checksums = ChecksumCapabilities::default();
    let mut buffer = vec![0u8; len];
    let mut frame = EthernetFrame::new_unchecked(&mut buffer[..]);
    ethernet.emit(&mut frame);
    let mut packet = Ipv4Packet::new_unchecked(frame.payload_mut());
    ip.emit(&mut packet, &checksums);
    let mut datagram = UdpPacket::new_unchecked(packet.payload_mut());
    udp.emit(
        &mut datagram,
        &IpAddress::Ipv4(*src.ip()),
        &IpAddress::Ipv4(*dst.ip()),
        payload.len(),
        |body| body.copy_from_slice(payload),
        &checksums,
    );
    Some(buffer)
}

pub const BROADCAST_IP: Ipv4Addr = Ipv4Addr::BROADCAST;

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST_MAC: [u8; MAC_LEN] = [2, 0, 0, 0, 0, 1];
    const GATEWAY_MAC: [u8; MAC_LEN] = [2, 0, 0, 0, 0, 2];
    const ETHERNET_HEADER_LEN: usize = 14;
    const IPV4_HEADER_LEN: usize = 20;

    fn guest() -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 15), 40000)
    }

    fn remote() -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 7), 27015)
    }

    #[test]
    fn udp_frame_round_trips_through_classify() {
        let frame = udp_frame(GUEST_MAC, GATEWAY_MAC, guest(), remote(), b"hello").unwrap();
        assert_eq!(
            classify(&frame),
            Ingress::Udp(UdpDatagram {
                src: guest(),
                dst: remote(),
                payload: b"hello"
            })
        );
    }

    #[test]
    fn udp_payload_that_needs_a_second_frame_is_refused() {
        let largest = MAX_FRAME_LEN - ETHERNET_HEADER_LEN - IPV4_HEADER_LEN - UDP_HEADER_LEN;
        assert!(udp_frame(GUEST_MAC, GATEWAY_MAC, guest(), remote(), &vec![0; largest]).is_some());
        assert!(
            udp_frame(
                GUEST_MAC,
                GATEWAY_MAC,
                guest(),
                remote(),
                &vec![0; largest + 1]
            )
            .is_none()
        );
    }

    #[test]
    fn empty_udp_payload_is_valid() {
        let frame = udp_frame(GUEST_MAC, GATEWAY_MAC, guest(), remote(), b"").unwrap();
        assert_eq!(
            classify(&frame),
            Ingress::Udp(UdpDatagram {
                src: guest(),
                dst: remote(),
                payload: b""
            })
        );
    }

    #[test]
    fn ethernet_padding_after_a_short_datagram_is_not_part_of_the_payload() {
        let mut frame = udp_frame(GUEST_MAC, GATEWAY_MAC, guest(), remote(), b"hi").unwrap();
        frame.resize(60, 0xee);
        assert_eq!(
            classify(&frame),
            Ingress::Udp(UdpDatagram {
                src: guest(),
                dst: remote(),
                payload: b"hi"
            })
        );
    }

    #[test]
    fn arp_goes_to_the_stack_and_unknown_ethertypes_are_dropped() {
        let mut arp = vec![0u8; 60];
        arp[12..14].copy_from_slice(&[0x08, 0x06]);
        assert_eq!(classify(&arp), Ingress::Stack);
        let mut ipv6 = vec![0u8; 60];
        ipv6[12..14].copy_from_slice(&[0x86, 0xdd]);
        assert_eq!(classify(&ipv6), Ingress::Drop);
    }

    #[test]
    fn truncated_and_fragmented_frames_are_dropped() {
        let frame = udp_frame(GUEST_MAC, GATEWAY_MAC, guest(), remote(), b"payload").unwrap();
        for cut in 0..frame.len() {
            assert_eq!(classify(&frame[..cut]), Ingress::Drop, "cut at {cut}");
        }
        // Set "more fragments" in the IPv4 flags field.
        let mut fragment = frame.clone();
        fragment[ETHERNET_HEADER_LEN + 6] |= 0x20;
        assert_eq!(classify(&fragment), Ingress::Drop);
    }
}
