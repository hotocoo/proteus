//! Which guest traffic may leave the machine.
//!
//! The policy sees three things: a DNS name being looked up, a connection's destination address, and the
//! server name in a TLS ClientHello. It cannot see inside encrypted traffic, and it cannot tell tracking
//! apart from content when both are served by the same host.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4};

/// Port of HTTPS over TCP and of QUIC over UDP.
const HTTPS_PORT: u16 = 443;
/// Port of DNS over TLS and of DNS over QUIC.
const DNS_OVER_TLS_PORT: u16 = 853;
/// Longest DNS name in text form.
const MAX_NAME_LEN: usize = 253;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Answer locally so that nothing about this flow reaches the host's network.
    Sinkhole,
}

pub trait FlowPolicy: Send + Sync {
    fn dns(&self, qname: &str) -> Verdict;
    /// Judged when the guest first tries to reach `dst`, before any payload is seen.
    fn connect(&self, dst: SocketAddrV4, proto: Proto) -> Verdict;
    /// Judged once a TCP flow's first bytes are in. `sni` is `None` for anything but a TLS ClientHello
    /// that names a server.
    fn tls_sni(&self, dst: SocketAddrV4, sni: Option<&str>) -> Verdict;
}

/// Lowercases, drops a trailing dot and a leading `*.`, and rejects anything that is not a
/// multi-label DNS name. Single labels are refused so that a stray `com` cannot block everything.
fn normalise(domain: &str) -> Option<String> {
    let domain = domain.trim().trim_end_matches('.');
    let domain = domain
        .strip_prefix("*.")
        .unwrap_or(domain)
        .to_ascii_lowercase();
    let label_ok = |label: &str| {
        !label.is_empty()
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    };
    let valid = domain.len() <= MAX_NAME_LEN
        && domain.contains('.')
        && domain.split('.').all(label_ok)
        && domain.parse::<Ipv4Addr>().is_err();
    valid.then_some(domain)
}

/// A set of domains that also matches every subdomain of its members.
#[derive(Debug, Default)]
pub struct DomainSet(HashSet<String>);

impl DomainSet {
    /// Adds one domain. Returns false if the text is not a usable domain name.
    pub fn insert(&mut self, domain: &str) -> bool {
        normalise(domain)
            .map(|domain| self.0.insert(domain))
            .is_some()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// True if `name` or any parent domain of it is in the set.
    pub fn contains(&self, name: &str) -> bool {
        let Some(name) = normalise(name) else {
            return false;
        };
        let mut rest = name.as_str();
        loop {
            if self.0.contains(rest) {
                return true;
            }
            match rest.split_once('.') {
                Some((_, parent)) => rest = parent,
                None => return false,
            }
        }
    }

    /// Adds every domain found in a list file. Understands the three common formats, one entry per line:
    /// plain domains, hosts files (`0.0.0.0 example.com`), and adblock domain rules (`||example.com^`).
    /// Comments (`#`, `!`) and lines it does not understand are skipped. Returns how many were added.
    pub fn extend_from_list(&mut self, text: &str) -> usize {
        let before = self.len();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() || line.starts_with('!') {
                continue;
            }
            let candidate = match line.strip_prefix("||") {
                // An adblock rule that is only "this domain and its subdomains".
                Some(rule) => rule.strip_suffix('^'),
                None => {
                    let mut fields = line.split_whitespace();
                    match (fields.next(), fields.next(), fields.next()) {
                        (Some(domain), None, _) => Some(domain),
                        (Some(address), Some(domain), None)
                            if address.parse::<Ipv4Addr>().is_ok() =>
                        {
                            Some(domain)
                        }
                        _ => None,
                    }
                }
            };
            if let Some(domain) = candidate {
                self.insert(domain);
            }
        }
        self.len() - before
    }
}

/// The policy the emulator ships with: block lists, a list that always wins, and port rules for
/// transports that would hide the server name.
#[derive(Debug, Default)]
pub struct Sinkhole {
    /// Trackers, ad networks and public DNS-over-HTTPS resolvers.
    pub blocked: DomainSet,
    /// Hosts that must stay reachable (sign-in, push). Wins over `blocked`.
    pub required: DomainSet,
    /// Drop QUIC so that clients fall back to TLS over TCP, where the server name is visible.
    pub drop_quic: bool,
    /// Drop DNS over TLS and DNS over QUIC, which would bypass the DNS check.
    pub drop_encrypted_dns: bool,
}

impl Sinkhole {
    fn name(&self, name: &str) -> Verdict {
        if !self.required.contains(name) && self.blocked.contains(name) {
            Verdict::Sinkhole
        } else {
            Verdict::Allow
        }
    }
}

impl FlowPolicy for Sinkhole {
    fn dns(&self, qname: &str) -> Verdict {
        self.name(qname)
    }

    fn connect(&self, dst: SocketAddrV4, proto: Proto) -> Verdict {
        let dropped = match (proto, dst.port()) {
            (Proto::Udp, HTTPS_PORT) => self.drop_quic,
            (_, DNS_OVER_TLS_PORT) => self.drop_encrypted_dns,
            _ => false,
        };
        if dropped {
            Verdict::Sinkhole
        } else {
            Verdict::Allow
        }
    }

    fn tls_sni(&self, _dst: SocketAddrV4, sni: Option<&str>) -> Verdict {
        sni.map_or(Verdict::Allow, |name| self.name(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(domains: &[&str]) -> DomainSet {
        let mut set = DomainSet::default();
        for domain in domains {
            assert!(set.insert(domain), "{domain}");
        }
        set
    }

    fn addr(port: u16) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), port)
    }

    #[test]
    fn a_domain_matches_itself_and_its_subdomains_only() {
        let set = set(&["tracker.example"]);
        assert!(set.contains("tracker.example"));
        assert!(set.contains("a.b.tracker.example"));
        assert!(set.contains("TRACKER.Example."));
        assert!(!set.contains("example"));
        assert!(!set.contains("nottracker.example"));
        assert!(!set.contains("tracker.example.org"));
    }

    #[test]
    fn unusable_entries_are_refused() {
        let mut set = DomainSet::default();
        for bad in [
            "",
            "com",
            "localhost",
            "0.0.0.0",
            "has space.example",
            "bad/char.example",
            "a..b",
        ] {
            assert!(!set.insert(bad), "{bad:?}");
        }
        assert!(!set.insert(&format!("{}.example", "a".repeat(MAX_NAME_LEN))));
        assert!(set.is_empty());
        assert!(!set.contains("not a name"));
    }

    #[test]
    fn list_formats_are_understood_and_noise_is_skipped() {
        let mut set = DomainSet::default();
        let added = set.extend_from_list(
            "# a comment\n\
             ! another comment\n\
             plain.example\n\
             0.0.0.0 hosts.example   # trailing comment\n\
             127.0.0.1 localhost\n\
             ||adblock.example^\n\
             ||adblock-with-options.example^$third-party\n\
             @@||exception.example^\n\
             *.wildcard.example\n\
             three fields here\n\
             \n",
        );
        assert_eq!(added, 4);
        for domain in [
            "plain.example",
            "hosts.example",
            "adblock.example",
            "x.wildcard.example",
        ] {
            assert!(set.contains(domain), "{domain}");
        }
        for domain in [
            "adblock-with-options.example",
            "exception.example",
            "localhost",
        ] {
            assert!(!set.contains(domain), "{domain}");
        }
    }

    #[test]
    fn required_names_win_over_blocked_ones() {
        let policy = Sinkhole {
            blocked: set(&["google.example", "ads.example"]),
            required: set(&["accounts.google.example"]),
            ..Sinkhole::default()
        };
        assert_eq!(policy.dns("ads.example"), Verdict::Sinkhole);
        assert_eq!(policy.dns("tracking.google.example"), Verdict::Sinkhole);
        assert_eq!(policy.dns("accounts.google.example"), Verdict::Allow);
        assert_eq!(policy.dns("other.example"), Verdict::Allow);
        assert_eq!(
            policy.tls_sni(addr(443), Some("cdn.ads.example")),
            Verdict::Sinkhole
        );
        assert_eq!(
            policy.tls_sni(addr(443), Some("accounts.google.example")),
            Verdict::Allow
        );
        assert_eq!(policy.tls_sni(addr(443), None), Verdict::Allow);
    }

    #[test]
    fn port_rules_apply_only_when_switched_on() {
        let open = Sinkhole::default();
        let strict = Sinkhole {
            drop_quic: true,
            drop_encrypted_dns: true,
            ..Sinkhole::default()
        };
        assert_eq!(open.connect(addr(443), Proto::Udp), Verdict::Allow);
        assert_eq!(strict.connect(addr(443), Proto::Udp), Verdict::Sinkhole);
        assert_eq!(strict.connect(addr(443), Proto::Tcp), Verdict::Allow);
        assert_eq!(strict.connect(addr(853), Proto::Tcp), Verdict::Sinkhole);
        assert_eq!(strict.connect(addr(853), Proto::Udp), Verdict::Sinkhole);
        assert_eq!(strict.connect(addr(27015), Proto::Udp), Verdict::Allow);
    }
}
