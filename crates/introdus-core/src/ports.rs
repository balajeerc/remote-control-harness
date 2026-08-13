//! Parsing and validation of `EXTRA_PORTS` entries — each either a single port
//! (published host:container on the same number) or `host:container` to remap.
//! Mirrors the validation in `launch_dev_container.sh`.

use anyhow::{bail, Result};
use std::net::TcpListener;

/// Find a free TCP port at or above `base`, skipping any in `avoid`, by trying to
/// bind `0.0.0.0:port` on the host — which also detects ports already published
/// by other introdus containers. The listener is dropped immediately, so the port
/// is free for the container to publish (a small TOCTOU window the caller closes
/// by persisting the pick and retrying). Errors if none is free in a 200-port
/// span. Used to assign each direct-mode paseo daemon a stable, non-colliding
/// port from [`crate::config::PASEO_PORT_BASE`].
pub fn pick_free_port(base: u16, avoid: &[u16]) -> Result<u16> {
    let end = base.saturating_add(200);
    for port in base..end {
        if avoid.contains(&port) {
            continue;
        }
        if port_is_free(port) {
            return Ok(port);
        }
    }
    bail!("no free port available in {base}..{end}");
}

/// True if `port` can be published on the host right now, by test-binding
/// `0.0.0.0:port` and dropping the listener immediately. Deliberately checks the
/// wildcard address even for a loopback publish: binding `0.0.0.0` fails if
/// *any* interface holds the port, so this errs toward reporting a conflict — the
/// safe direction, since a false "busy" costs a remap and a false "free" costs a
/// failed launch.
pub fn port_is_free(port: u16) -> bool {
    TcpListener::bind(("0.0.0.0", port)).is_ok()
}

/// How the webapp publish reads to a human — the launch line and the panel
/// header share this so a remapped host port is described identically in both.
pub fn publish_desc(container_port: u16, host_port: Option<u16>) -> String {
    match host_port {
        Some(h) if h != container_port => {
            format!("port {h} on the host -> {container_port} in the container")
        }
        _ => format!("port {container_port}"),
    }
}

/// The `--format` template for [`port_owner`]: tab-separated name / port list.
pub const PS_PORTS_FORMAT: &str = "{{.Names}}\t{{.Ports}}";

/// Find which running container publishes host `port`, given the output of
/// `podman ps --format {PS_PORTS_FORMAT}`. Each `{{.Ports}}` cell is a
/// comma-separated list of `<ip>:<host>-><container>/<proto>` mappings (podman
/// omits the `<ip>:` prefix when it published on the wildcard address). Returns
/// the first container publishing `port`, or `None` when nothing here does —
/// which, for a port that failed [`port_is_free`], means a non-podman process
/// holds it.
pub fn port_owner(ps_output: &str, port: u16) -> Option<String> {
    for line in ps_output.lines() {
        let Some((name, mappings)) = line.split_once('\t') else {
            continue;
        };
        for mapping in mappings.split(',') {
            // "127.0.0.1:3000->3000/tcp" -> host side "127.0.0.1:3000" -> "3000"
            let host_side = mapping.trim().split("->").next().unwrap_or("").trim();
            let host_port = host_side.rsplit(':').next().unwrap_or("");
            if host_port.parse::<u16>() == Ok(port) {
                return Some(name.trim().to_owned());
            }
        }
    }
    None
}

/// Parse `EXTRA_PORTS` entries into `(host, container)` pairs, rejecting
/// malformed entries, out-of-range ports, and any host port colliding with the
/// webapp port.
pub fn parse_extra_ports(entries: &[String], webapp_port: u16) -> Result<Vec<(u16, u16)>> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let (host, container) = match entry.split_once(':') {
            Some((h, c)) => (parse_port(h, entry)?, parse_port(c, entry)?),
            None => {
                let p = parse_port(entry, entry)?;
                (p, p)
            }
        };
        if host == webapp_port {
            bail!("EXTRA_PORTS host port {host} collides with WEBAPP_PORT");
        }
        out.push((host, container));
    }
    Ok(out)
}

fn parse_port(s: &str, entry: &str) -> Result<u16> {
    match s.parse::<u32>() {
        Ok(p) if (1..=65535).contains(&p) => Ok(p as u16),
        _ => bail!("EXTRA_PORTS entry is not a valid port or host:container mapping: '{entry}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ta45_parses_single_and_mapped() {
        let e = vec!["8123".to_owned(), "16379:6379".to_owned()];
        assert_eq!(
            parse_extra_ports(&e, 3000).unwrap(),
            vec![(8123, 8123), (16379, 6379)]
        );
    }

    #[test]
    fn ta45_rejects_bad_and_colliding() {
        assert!(parse_extra_ports(&["0".to_owned()], 3000).is_err());
        assert!(parse_extra_ports(&["70000".to_owned()], 3000).is_err());
        assert!(parse_extra_ports(&["abc".to_owned()], 3000).is_err());
        assert!(parse_extra_ports(&["3000".to_owned()], 3000).is_err());
    }

    #[test]
    fn ta163_pick_free_port_returns_bindable_and_skips_avoid() {
        // Occupy a port, then ask starting there: the picker must skip the taken
        // one and return a different, actually-bindable port at/above the base.
        let occupied = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let taken = occupied.local_addr().unwrap().port();
        let got = pick_free_port(taken, &[]).unwrap();
        assert!(got >= taken);
        assert_ne!(got, taken, "must skip the port that is already bound");
        // The returned port is genuinely free (the picker dropped its test bind).
        assert!(TcpListener::bind(("0.0.0.0", got)).is_ok());
        // An explicit avoid entry is honored too.
        let got2 = pick_free_port(taken, &[got]).unwrap();
        assert_ne!(got2, got);
        assert_ne!(got2, taken);
    }

    #[test]
    fn ta174_port_is_free_tracks_a_live_bind() {
        let occupied = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let taken = occupied.local_addr().unwrap().port();
        assert!(!port_is_free(taken), "a bound port must read as busy");
        drop(occupied);
        assert!(port_is_free(taken), "a released port must read as free");
    }

    #[test]
    fn ta176_publish_desc_only_spells_out_a_remap() {
        assert_eq!(publish_desc(3000, None), "port 3000");
        assert_eq!(publish_desc(3000, Some(3000)), "port 3000");
        assert_eq!(
            publish_desc(3000, Some(3001)),
            "port 3001 on the host -> 3000 in the container"
        );
    }

    #[test]
    fn ta175_port_owner_finds_the_publishing_container() {
        let ps = "introdus-a\t127.0.0.1:3000->3000/tcp, 0.0.0.0:20190->20190/tcp\n\
                  introdus-b\t127.0.0.1:8123->8123/tcp\n";
        assert_eq!(port_owner(ps, 3000).as_deref(), Some("introdus-a"));
        assert_eq!(port_owner(ps, 20190).as_deref(), Some("introdus-a"));
        assert_eq!(port_owner(ps, 8123).as_deref(), Some("introdus-b"));
        // The container side of a mapping is not the host side.
        assert_eq!(
            port_owner("introdus-a\t127.0.0.1:16379->6379/tcp", 6379),
            None
        );
        assert_eq!(port_owner(ps, 9999), None);
    }

    #[test]
    fn ta175_port_owner_tolerates_odd_rows() {
        // No-ip mapping, an empty port cell, and a malformed line.
        let ps = "introdus-a\t3000->3000/tcp\nintrodus-b\t\ngarbage-without-a-tab\n";
        assert_eq!(port_owner(ps, 3000).as_deref(), Some("introdus-a"));
        assert_eq!(port_owner(ps, 1), None);
        assert_eq!(port_owner("", 3000), None);
    }
}
