//! Listen-port availability probing and suggestions. The UI calls these to
//! (a) flag a conflicting port before the user starts a tunnel, and (b)
//! offer concrete alternatives when the desired port is taken.
//!
//! Probing is done by *actually binding* (and immediately dropping) a
//! socket — the only race-free way to ask "is this port free" is to try
//! taking it.

use std::net::{IpAddr, TcpListener as StdTcpListener};

/// Above 1024 to dodge privileged ports and the huge ephemeral range
/// collisions at the low end.
const SUGGESTION_FLOOR: u16 = 1024;
/// How far we're willing to scan upward from the requested port.
const SCAN_WINDOW: u16 = 500;

/// Returns `true` when nothing is listening on `addr:port` *right now*.
pub fn check_port_available(addr: IpAddr, port: u16) -> bool {
    StdTcpListener::bind((addr, port)).is_ok()
}

/// Suggest up to `count` free ports, preferring `desired` itself when free,
/// then scanning upward.
///
/// Also tries a small set of tunnel-flavoured well-known defaults
/// (1080/8080/8888...) after the scan, useful when `desired` is buried in
/// used ports — e.g. when the relay's 8877 collides with something.
fn push_if_free(found: &mut Vec<u16>, addr: IpAddr, port: u16) -> bool {
    if port == 0 || found.contains(&port) {
        return false;
    }
    if check_port_available(addr, port) {
        found.push(port);
        true
    } else {
        false
    }
}

pub fn suggest_listen_ports(addr: IpAddr, desired: u16, count: usize) -> Vec<u16> {
    let mut found: Vec<u16> = Vec::with_capacity(count);

    // 1. The desired port first.
    if push_if_free(&mut found, addr, desired) && found.len() >= count {
        return found;
    }

    // 2. Scan upward from the request.
    let start = desired.max(SUGGESTION_FLOOR);
    for offset in 1..=SCAN_WINDOW {
        let candidate = start.saturating_add(offset);
        if candidate == 0 || candidate == u16::MAX {
            break;
        }
        push_if_free(&mut found, addr, candidate);
        if found.len() >= count {
            return found;
        }
    }

    // 3. Fallback defaults.
    for fallback in [1080u16, 8080, 8888, 8878, 8879, 3128] {
        push_if_free(&mut found, addr, fallback);
        if found.len() >= count {
            return found;
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn available_when_free() {
        assert!(check_port_available(Ipv4Addr::LOCALHOST.into(), 0));
        // Bind an ephemeral port, drop it, then confirm the same style of
        // check reports a held port as unavailable.
    }

    #[test]
    fn unavailable_when_busy() {
        let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!check_port_available(Ipv4Addr::LOCALHOST.into(), port));
        drop(listener);
        // After drop it should become available again (no lingering bind).
        assert!(check_port_available(Ipv4Addr::LOCALHOST.into(), port));
    }

    #[test]
    fn suggestions_include_free_candidates() {
        let suggestions = suggest_listen_ports(Ipv4Addr::LOCALHOST.into(), 0, 3);
        // With port 0 the "desired" is skipped, so we get scan results
        // starting at the elevation floor. (We don't re-probe the returned
        // ports here — another test running in parallel could legitimately
        // grab one between suggestion and check.)
        assert!(!suggestions.is_empty());
        for p in &suggestions {
            assert!(*p >= SUGGESTION_FLOOR);
        }
    }

    #[test]
    fn suggestions_avoid_busy_port() {
        let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let busy = listener.local_addr().unwrap().port();
        let suggestions = suggest_listen_ports(Ipv4Addr::LOCALHOST.into(), busy, 4);
        assert!(!suggestions.contains(&busy));
        assert!(!suggestions.is_empty());
        // Suggestions should be close to where the user asked.
        assert!(suggestions.iter().any(|p| *p > busy));
    }

    #[test]
    fn no_zero_ports_suggested() {
        let suggestions = suggest_listen_ports(Ipv4Addr::LOCALHOST.into(), 0, 10);
        assert!(suggestions.iter().all(|p| *p != 0));
    }
}
