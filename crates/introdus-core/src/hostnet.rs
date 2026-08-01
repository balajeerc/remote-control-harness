//! Host network address discovery — which IPv4 address a client on your laptop
//! should dial to reach a port published on this container host.
//!
//! Used by the direct-mode paseo connect action: the daemon's port is published
//! on the host's all-interfaces address, so the URL we show has to name a *real*
//! reachable address. A **tailscale/VPN address wins** when there is one (that's
//! the network direct mode is meant for — see `05_security.md`); otherwise the
//! first ordinary LAN address, with podman/docker/bridge interfaces ranked last
//! since nothing outside this host can route to them.
//!
//! The parsing is pure and unit-tested; [`detect`] is the thin shell that runs
//! `ip -o -4 addr show` (falling back to `hostname -I`, which loses interface
//! names but still yields addresses).

use crate::process::Cmd;

/// One IPv4 address on a host interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addr {
    /// The interface it is configured on (empty when the source didn't name one).
    pub iface: String,
    /// The dotted-quad address, without the prefix length.
    pub ip: String,
}

impl Addr {
    /// Whether this looks like a tailscale address: the interface tailscaled
    /// creates, or an address in tailscale's CGNAT range `100.64.0.0/10`
    /// (which is how we recognize it when the interface name is unknown, e.g.
    /// from the `hostname -I` fallback or a userspace-networking setup).
    pub fn is_tailscale(&self) -> bool {
        self.iface.starts_with("tailscale") || in_cgnat(&self.ip)
    }

    /// A short "where this came from" note for the UI.
    pub fn source(&self) -> String {
        match (self.is_tailscale(), self.iface.is_empty()) {
            (true, true) => "tailscale (CGNAT range)".to_owned(),
            (true, false) => format!("tailscale interface {}", self.iface),
            (false, true) => "this host".to_owned(),
            (false, false) => format!("interface {}", self.iface),
        }
    }

    /// Ranking for [`pick`] — lower is preferred: tailscale/VPN first, then an
    /// ordinary interface, then a container/bridge interface nothing off-host
    /// can route to.
    fn rank(&self) -> u8 {
        if self.is_tailscale() {
            0
        } else if is_virtual(&self.iface) {
            2
        } else {
            1
        }
    }
}

/// Interfaces created by container/VM tooling — routable only on this host, so
/// they are the last resort when picking an address to hand a remote client.
fn is_virtual(iface: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "podman", "docker", "cni-", "flannel", "veth", "virbr", "vnet", "br-", "kube",
    ];
    PREFIXES.iter().any(|p| iface.starts_with(p))
}

/// Whether `ip` is in the `100.64.0.0/10` carrier-grade-NAT range tailscale
/// assigns from.
fn in_cgnat(ip: &str) -> bool {
    let mut parts = ip.split('.').map(str::parse::<u8>);
    match (parts.next(), parts.next()) {
        (Some(Ok(100)), Some(Ok(second))) => (64..=127).contains(&second),
        _ => false,
    }
}

/// Addresses we never offer: loopback and link-local (169.254/16) can't be
/// dialed from another machine.
fn is_usable(ip: &str) -> bool {
    !ip.starts_with("127.") && !ip.starts_with("169.254.") && ip.contains('.')
}

/// Parse `ip -o -4 addr show` output. Each line is
/// `2: eth0    inet 192.168.1.5/24 brd … scope global eth0\       valid_lft …`,
/// so the interface is the second field and the address follows the `inet`
/// token. Unusable (loopback / link-local) addresses are dropped.
pub fn parse_ip_addr_show(out: &str) -> Vec<Addr> {
    let mut addrs = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let Some(i) = f.iter().position(|t| *t == "inet") else {
            continue;
        };
        let (Some(iface), Some(cidr)) = (f.get(1), f.get(i + 1)) else {
            continue;
        };
        let ip = cidr.split('/').next().unwrap_or_default();
        if is_usable(ip) {
            addrs.push(Addr {
                iface: (*iface).to_owned(),
                ip: ip.to_owned(),
            });
        }
    }
    addrs
}

/// Parse `hostname -I` output — a space-separated address list with no
/// interface names (the fallback when `ip` isn't on PATH).
pub fn parse_hostname_i(out: &str) -> Vec<Addr> {
    out.split_whitespace()
        .filter(|ip| is_usable(ip))
        .map(|ip| Addr {
            iface: String::new(),
            ip: ip.to_owned(),
        })
        .collect()
}

/// Pick the address a remote client should dial: the best-ranked one, ties
/// broken by the order the OS listed them (`min_by_key` keeps the first).
pub fn pick(addrs: &[Addr]) -> Option<Addr> {
    addrs.iter().min_by_key(|a| a.rank()).cloned()
}

/// Discover this host's best client-reachable IPv4 address, or `None` when the
/// host has no usable one (or neither probe is available) — the caller then
/// shows a `<host-ip>` placeholder rather than a wrong address.
pub fn detect() -> Option<Addr> {
    // `ip` is the better source (it names the interface); `hostname -I` is the
    // fallback for a host without iproute2.
    let from_ip = Cmd::new("ip")
        .args(["-o", "-4", "addr", "show"])
        .stdout_quiet()
        .ok()
        .and_then(|out| pick(&parse_ip_addr_show(&out)));
    from_ip.or_else(|| {
        let out = Cmd::new("hostname").arg("-I").stdout_quiet().ok()?;
        pick(&parse_hostname_i(&out))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP_OUT: &str = "\
1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
2: eth0    inet 192.168.1.5/24 brd 192.168.1.255 scope global eth0\\       valid_lft forever
3: podman0    inet 10.88.0.1/16 brd 10.88.255.255 scope global podman0\\       valid_lft forever
5: tailscale0    inet 100.117.172.96/32 scope global tailscale0\\       valid_lft forever
";

    #[test]
    fn ta168_picks_the_tailscale_address_over_lan_and_bridges() {
        let addrs = parse_ip_addr_show(IP_OUT);
        // Loopback is dropped; the three routable ones survive with their ifaces.
        assert_eq!(addrs.len(), 3, "{addrs:?}");
        assert!(!addrs.iter().any(|a| a.ip.starts_with("127.")));
        let best = pick(&addrs).unwrap();
        assert_eq!(best.ip, "100.117.172.96");
        assert!(best.source().contains("tailscale0"), "{}", best.source());
    }

    #[test]
    fn ta168_falls_back_to_lan_then_bridge_addresses() {
        // No tailscale: the ordinary LAN interface beats the podman bridge even
        // though the bridge was listed first.
        let out = "3: podman0    inet 10.88.0.1/16 scope global podman0\n\
                   2: eth0    inet 192.168.1.5/24 scope global eth0\n";
        assert_eq!(pick(&parse_ip_addr_show(out)).unwrap().ip, "192.168.1.5");
        // Bridge-only host: better a host-local address than nothing.
        let only = "3: podman0    inet 10.88.0.1/16 scope global podman0\n";
        assert_eq!(pick(&parse_ip_addr_show(only)).unwrap().ip, "10.88.0.1");
        // Nothing usable at all → no address (the caller shows a placeholder).
        assert!(pick(&parse_ip_addr_show(
            "1: lo    inet 127.0.0.1/8 scope host lo\n"
        ))
        .is_none());
    }

    #[test]
    fn ta168_hostname_fallback_recognizes_tailscale_by_cgnat_range() {
        // `hostname -I` has no interface names, so the CGNAT range is the only
        // tailscale signal — and it must still win.
        let addrs = parse_hostname_i("192.168.1.5 100.117.172.96 172.17.0.1\n");
        assert_eq!(addrs.len(), 3);
        let best = pick(&addrs).unwrap();
        assert_eq!(best.ip, "100.117.172.96");
        assert!(best.is_tailscale());
        assert!(best.source().contains("tailscale"), "{}", best.source());
        // 100.x outside 64..=127 in the second octet is NOT CGNAT.
        assert!(!in_cgnat("100.200.0.1"));
        assert!(!in_cgnat("10.0.0.1"));
    }
}
