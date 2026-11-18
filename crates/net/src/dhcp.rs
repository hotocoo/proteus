//! A DHCP server for exactly one client: it always hands the guest its one configured address.

use std::net::Ipv4Addr;

use smoltcp::wire::{DhcpMessageType, DhcpOption, DhcpPacket, DhcpRepr};

use crate::config::NetConfig;

/// DHCP option 6, the list of DNS servers.
const OPTION_DNS_SERVERS: u8 = 6;

fn netmask(prefix_len: u8) -> Ipv4Addr {
    let host_bits = u32::BITS.saturating_sub(u32::from(prefix_len));
    Ipv4Addr::from(u32::MAX.checked_shl(host_bits).unwrap_or(0))
}

/// The server's answer to a client message, as a UDP payload, or `None` if it needs no answer.
pub fn reply(request: &[u8], cfg: &NetConfig) -> Option<Vec<u8>> {
    let packet = DhcpPacket::new_checked(request).ok()?;
    let request = DhcpRepr::parse(&packet).ok()?;
    let message_type = match request.message_type {
        DhcpMessageType::Discover => DhcpMessageType::Offer,
        // A client asking for some other address is told no, and starts over.
        DhcpMessageType::Request => match request.requested_ip {
            Some(wanted) if wanted != cfg.guest_ip => DhcpMessageType::Nak,
            _ => DhcpMessageType::Ack,
        },
        _ => return None,
    };
    let dns = cfg.gateway_ip.octets();
    let options = [DhcpOption {
        kind: OPTION_DNS_SERVERS,
        data: &dns,
    }];
    let reply = DhcpRepr {
        message_type,
        transaction_id: request.transaction_id,
        secs: 0,
        client_hardware_address: request.client_hardware_address,
        client_ip: Ipv4Addr::UNSPECIFIED,
        your_ip: cfg.guest_ip,
        server_ip: cfg.gateway_ip,
        router: Some(cfg.gateway_ip),
        subnet_mask: Some(netmask(cfg.prefix_len)),
        relay_agent_ip: Ipv4Addr::UNSPECIFIED,
        broadcast: true,
        requested_ip: None,
        client_identifier: None,
        server_identifier: Some(cfg.gateway_ip),
        parameter_request_list: None,
        dns_servers: None,
        max_size: None,
        lease_duration: Some(cfg.dhcp_lease_secs),
        renew_duration: None,
        rebind_duration: None,
        additional_options: &options,
    };
    let mut out = vec![0u8; reply.buffer_len()];
    reply
        .emit(&mut DhcpPacket::new_unchecked(&mut out[..]))
        .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use smoltcp::wire::EthernetAddress;

    use super::*;

    const CLIENT_MAC: EthernetAddress = EthernetAddress([2, 0, 0, 0, 0, 1]);
    const XID: u32 = 0x1234_5678;

    fn client_message(message_type: DhcpMessageType, requested_ip: Option<Ipv4Addr>) -> Vec<u8> {
        let repr = DhcpRepr {
            message_type,
            transaction_id: XID,
            secs: 0,
            client_hardware_address: CLIENT_MAC,
            client_ip: Ipv4Addr::UNSPECIFIED,
            your_ip: Ipv4Addr::UNSPECIFIED,
            server_ip: Ipv4Addr::UNSPECIFIED,
            router: None,
            subnet_mask: None,
            relay_agent_ip: Ipv4Addr::UNSPECIFIED,
            broadcast: false,
            requested_ip,
            client_identifier: Some(CLIENT_MAC),
            server_identifier: None,
            parameter_request_list: None,
            dns_servers: None,
            max_size: None,
            lease_duration: None,
            renew_duration: None,
            rebind_duration: None,
            additional_options: &[],
        };
        let mut out = vec![0u8; repr.buffer_len()];
        repr.emit(&mut DhcpPacket::new_unchecked(&mut out[..]))
            .unwrap();
        out
    }

    fn answer(message_type: DhcpMessageType, requested_ip: Option<Ipv4Addr>) -> Option<Vec<u8>> {
        reply(
            &client_message(message_type, requested_ip),
            &NetConfig::default(),
        )
    }

    #[test]
    fn discover_is_offered_the_guest_address_with_router_mask_dns_and_lease() {
        let cfg = NetConfig::default();
        let bytes = answer(DhcpMessageType::Discover, None).unwrap();
        let packet = DhcpPacket::new_checked(&bytes[..]).unwrap();
        let offer = DhcpRepr::parse(&packet).unwrap();
        assert_eq!(offer.message_type, DhcpMessageType::Offer);
        assert_eq!(
            (offer.transaction_id, offer.client_hardware_address),
            (XID, CLIENT_MAC)
        );
        assert_eq!(offer.your_ip, cfg.guest_ip);
        assert_eq!(offer.router, Some(cfg.gateway_ip));
        assert_eq!(offer.subnet_mask, Some(Ipv4Addr::new(255, 255, 255, 0)));
        assert_eq!(offer.server_identifier, Some(cfg.gateway_ip));
        assert_eq!(offer.lease_duration, Some(cfg.dhcp_lease_secs));
        assert_eq!(offer.dns_servers.unwrap().as_slice(), [cfg.gateway_ip]);
    }

    #[test]
    fn request_is_acknowledged_unless_it_asks_for_another_address() {
        let cfg = NetConfig::default();
        let kind = |bytes: Vec<u8>| {
            DhcpRepr::parse(&DhcpPacket::new_checked(&bytes[..]).unwrap())
                .unwrap()
                .message_type
        };
        assert_eq!(
            kind(answer(DhcpMessageType::Request, Some(cfg.guest_ip)).unwrap()),
            DhcpMessageType::Ack
        );
        assert_eq!(
            kind(answer(DhcpMessageType::Request, None).unwrap()),
            DhcpMessageType::Ack
        );
        let other = Ipv4Addr::new(192, 168, 1, 50);
        assert_eq!(
            kind(answer(DhcpMessageType::Request, Some(other)).unwrap()),
            DhcpMessageType::Nak
        );
    }

    #[test]
    fn other_messages_and_garbage_get_no_answer() {
        assert!(answer(DhcpMessageType::Release, None).is_none());
        assert!(answer(DhcpMessageType::Offer, None).is_none());
        let good = client_message(DhcpMessageType::Discover, None);
        for cut in [0, 1, 100, good.len() - 1] {
            let _ = reply(&good[..cut], &NetConfig::default());
        }
        assert!(reply(&[0xff; 300], &NetConfig::default()).is_none());
    }

    #[test]
    fn netmask_covers_every_prefix_length() {
        assert_eq!(netmask(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(netmask(0), Ipv4Addr::UNSPECIFIED);
        assert_eq!(netmask(32), Ipv4Addr::BROADCAST);
        assert_eq!(netmask(40), Ipv4Addr::BROADCAST);
    }
}
