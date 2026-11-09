//! Network destination rules. No network I/O is performed here.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::error::{Error, Result};

/// One explicitly named host (no wildcards, no IP literals).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostPattern(String);

impl HostPattern {
    /// Validate an explicit lowercase DNS hostname.
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.is_empty()
            || raw.len() > 253
            || raw.ends_with('.')
            || raw.bytes().any(|b| b.is_ascii_uppercase())
            || raw.parse::<IpAddr>().is_ok()
        {
            return Err(Error::invalid_argument("ambiguous or malformed host"));
        }
        for label in raw.split('.') {
            let ok = !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !ok {
                return Err(Error::invalid_argument("malformed host label"));
            }
        }
        Ok(HostPattern(raw.to_string()))
    }
    /// Canonical host text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Inclusive port range; port 0 is never allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortRange {
    start: u16,
    end: u16,
}

impl PortRange {
    /// Single port.
    pub fn single(port: u16) -> Result<Self> {
        if port == 0 {
            return Err(Error::invalid_argument("port 0 is not allowed"));
        }
        Ok(Self {
            start: port,
            end: port,
        })
    }
    /// Inclusive range.
    pub fn range(start: u16, end: u16) -> Result<Self> {
        if start == 0 || end == 0 || start > end {
            return Err(Error::invalid_argument("invalid port range"));
        }
        Ok(Self { start, end })
    }
    /// Membership.
    #[must_use]
    pub fn contains(&self, port: u16) -> bool {
        (self.start..=self.end).contains(&port)
    }
    fn covered_by(&self, other: &Self) -> bool {
        other.start <= self.start && self.end <= other.end
    }
    fn intersect(&self, other: &Self) -> Option<Self> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        (start <= end).then_some(Self { start, end })
    }
}

/// A grant for exactly one host, a port set, and optionally private addresses.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NetworkRule {
    host: HostPattern,
    ports: BTreeSet<PortRange>,
    allow_private: bool,
}

impl NetworkRule {
    /// Host this rule applies to.
    #[must_use]
    pub fn host(&self) -> &HostPattern {
        &self.host
    }
    /// Whether non-global resolved addresses are permitted.
    #[must_use]
    pub fn allows_private(&self) -> bool {
        self.allow_private
    }
}

/// Deny-default set of explicit network grants.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkRules {
    rules: BTreeSet<NetworkRule>,
}

impl NetworkRules {
    /// Nothing permitted.
    #[must_use]
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// Permit one public-only host/port.
    pub fn allow(self, host: &str, port: u16) -> Result<Self> {
        self.insert(host, PortRange::single(port)?, false)
    }
    /// Permit one host/port including non-global resolved addresses.
    pub fn allow_private(self, host: &str, port: u16) -> Result<Self> {
        self.insert(host, PortRange::single(port)?, true)
    }
    /// Permit a public-only host over a port range.
    pub fn allow_range(self, host: &str, start: u16, end: u16) -> Result<Self> {
        self.insert(host, PortRange::range(start, end)?, false)
    }

    /// Permit a host over a port range including non-global addresses.
    pub fn allow_private_range(self, host: &str, start: u16, end: u16) -> Result<Self> {
        self.insert(host, PortRange::range(start, end)?, true)
    }

    fn insert(mut self, host: &str, port: PortRange, allow_private: bool) -> Result<Self> {
        self.rules.insert(NetworkRule {
            host: HostPattern::parse(host)?,
            ports: BTreeSet::from([port]),
            allow_private,
        });
        Ok(self)
    }

    /// Authorize the canonical hostname and port before performing DNS, so a
    /// denied hostname cannot leak guest data through resolution side effects.
    pub fn check_host(&self, host: &str, port: u16) -> Result<()> {
        let host = HostPattern::parse(host).map_err(|_| Error::network_denied("invalid host"))?;
        if port != 0
            && self
                .rules
                .iter()
                .any(|r| r.host == host && r.ports.iter().any(|p| p.contains(port)))
        {
            Ok(())
        } else {
            Err(Error::network_denied("connection destination denied"))
        }
    }

    /// Check the actual addresses to be connected; connectors must pin these
    /// addresses when dialing and repeat this check at every redirect.
    pub fn check(&self, host: &str, port: u16, resolved: &[IpAddr]) -> Result<()> {
        let host = HostPattern::parse(host).map_err(|_| Error::network_denied("invalid host"))?;
        if port == 0 || resolved.is_empty() {
            return Err(Error::network_denied("invalid connection destination"));
        }
        let private = resolved.iter().any(|ip| requires_private_grant(*ip));
        if self.rules.iter().any(|r| {
            r.host == host
                && r.ports.iter().any(|p| p.contains(port))
                && (!private || r.allow_private)
        }) {
            Ok(())
        } else {
            Err(Error::network_denied("connection destination denied"))
        }
    }

    /// A redirect must be re-checked exactly like a fresh destination.
    pub fn check_redirect(&self, host: &str, port: u16, resolved: &[IpAddr]) -> Result<()> {
        self.check(host, port, resolved)
    }

    /// Narrowing intersection (same host only).
    #[must_use]
    pub fn intersection(&self, other: &Self) -> Self {
        let mut rules = BTreeSet::new();
        for a in &self.rules {
            for b in &other.rules {
                if a.host != b.host {
                    continue;
                }
                let ports: BTreeSet<PortRange> = a
                    .ports
                    .iter()
                    .flat_map(|x| b.ports.iter().filter_map(move |y| x.intersect(y)))
                    .collect();
                if !ports.is_empty() {
                    rules.insert(NetworkRule {
                        host: a.host.clone(),
                        ports,
                        allow_private: a.allow_private && b.allow_private,
                    });
                }
            }
        }
        Self { rules }
    }

    /// Every grant here is covered by `other`.
    #[must_use]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        self.rules.iter().all(|r| {
            other.rules.iter().any(|o| {
                o.host == r.host
                    && (!r.allow_private || o.allow_private)
                    && r.ports
                        .iter()
                        .all(|p| o.ports.iter().any(|q| p.covered_by(q)))
            })
        })
    }
}

/// Conservative: returns `true` for anything not clearly globally routable.
#[must_use]
pub fn requires_private_grant(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4_is_global(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => !v4_is_global(v4),
            None => !v6_is_global(v6),
        },
    }
}

fn v4_is_global(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    let blocked = ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_unspecified()
        || o[0] == 0
        || o[0] >= 240
        || (o[0] == 100 && (64..=127).contains(&o[1]))
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 192 && o[1] == 0 && o[2] == 2)
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113);
    !blocked
}

fn v6_is_global(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // Only global unicast 2000::/3, excluding special/documentation prefixes.
    (s[0] & 0xe000) == 0x2000
        && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
        && s[0] != 0x2002
        && !((s[0] & 0xfff0) == 0x3ff0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn deny_default_and_private_resolution() {
        let n = NetworkRules::deny_all();
        assert_eq!(
            n.check("example.com", 443, &ips(&["93.184.216.34"]))
                .unwrap_err()
                .kind(),
            ErrorKind::NetworkDenied
        );
        let n = n.allow("example.com", 443).unwrap();
        n.check("example.com", 443, &ips(&["93.184.216.34"]))
            .unwrap();
        assert!(n.check("example.com", 443, &ips(&["127.0.0.1"])).is_err());
        assert!(n.check("example.com", 443, &ips(&["10.0.0.1"])).is_err());
        assert!(n.check("example.com", 443, &ips(&["::1"])).is_err());
        assert!(n.check("example.com", 443, &ips(&["fe80::1"])).is_err());
        assert!(n.check("example.com", 443, &[]).is_err());
        assert!(n.check("other.com", 443, &ips(&["93.184.216.34"])).is_err());
        assert!(n
            .check("example.com", 444, &ips(&["93.184.216.34"]))
            .is_err());
        assert!(n
            .check_redirect("example.com", 443, &ips(&["93.184.216.34", "10.1.1.1"]))
            .is_err());
    }

    #[test]
    fn explicit_private_grant_required() {
        let n = NetworkRules::deny_all()
            .allow_private("db.internal", 5432)
            .unwrap();
        n.check("db.internal", 5432, &ips(&["10.1.2.3"])).unwrap();
        n.check("db.internal", 5432, &ips(&["8.8.8.8"])).unwrap();
        let p = NetworkRules::deny_all().allow("db.internal", 5432).unwrap();
        assert!(p.check("db.internal", 5432, &ips(&["10.1.2.3"])).is_err());
        let r = NetworkRules::deny_all()
            .allow_private_range("db.internal", 5000, 6000)
            .unwrap();
        r.check("db.internal", 5500, &ips(&["192.168.1.9"]))
            .unwrap();
        assert!(r
            .check("db.internal", 6500, &ips(&["192.168.1.9"]))
            .is_err());
    }

    #[test]
    fn ambiguous_hosts_rejected() {
        for bad in [
            "",
            "EXAMPLE.com",
            "exa mple.com",
            "-a.com",
            "a-.com",
            "1.2.3.4",
            "::1",
            "a..b",
            "a.com.",
        ] {
            assert!(HostPattern::parse(bad).is_err(), "for {bad:?}");
        }
        assert!(PortRange::single(0).is_err());
        assert!(PortRange::range(80, 79).is_err());
    }

    #[test]
    fn subset_and_intersection() {
        let wide = NetworkRules::deny_all()
            .allow_range("a.com", 1, 1000)
            .unwrap();
        let narrow = NetworkRules::deny_all().allow("a.com", 443).unwrap();
        assert!(narrow.is_subset_of(&wide));
        assert!(!wide.is_subset_of(&narrow));
        assert_eq!(wide.intersection(&narrow), narrow);
        let privw = NetworkRules::deny_all()
            .allow_private("a.com", 443)
            .unwrap();
        assert!(privw
            .intersection(&narrow)
            .check("a.com", 443, &ips(&["10.0.0.1"]))
            .is_err());
    }

    #[test]
    fn conservative_ip_classification() {
        for s in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
        ] {
            assert!(requires_private_grant(s.parse().unwrap()), "{s}");
        }
        for s in ["93.184.216.34", "2606:2800:220:1::1"] {
            assert!(!requires_private_grant(s.parse().unwrap()), "{s}");
        }
    }
}
