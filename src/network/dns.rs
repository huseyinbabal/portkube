//! DNS proxy — resolves `<svc>.<namespace>` from the in-memory service map.
//! Non-matching queries forwarded to upstream.
//!
//! Example: `nginx.default` → 10.96.64.132

use anyhow::{Context, Result};
use std::net::Ipv4Addr;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use super::tun::ServiceMap;

const DNS_LISTEN_ADDR: &str = "127.0.0.53:53";
const DEFAULT_UPSTREAM: &str = "8.8.8.8:53";

fn get_upstream_dns() -> String {
    let saved = std::fs::read_to_string("/tmp/portkube-saved-dns").unwrap_or_default();
    let first = saved.lines().find(|l| !l.trim().is_empty()).map(|l| l.trim().to_string());
    match first {
        Some(ip) if !ip.is_empty() => format!("{ip}:53"),
        _ => DEFAULT_UPSTREAM.to_string(),
    }
}

fn parse_query_name(packet: &[u8]) -> Option<String> {
    if packet.len() < 17 {
        return None;
    }
    let mut pos = 12;
    let mut labels = Vec::new();
    loop {
        if pos >= packet.len() {
            return None;
        }
        let len = packet[pos] as usize;
        if len == 0 {
            break;
        }
        pos += 1;
        if pos + len > packet.len() {
            return None;
        }
        labels.push(std::str::from_utf8(&packet[pos..pos + len]).ok()?.to_string());
        pos += len;
    }
    Some(labels.join("."))
}

fn build_dns_response(query: &[u8], ip: Ipv4Addr) -> Vec<u8> {
    let qname_end = {
        let mut pos = 12;
        while pos < query.len() {
            let len = query[pos] as usize;
            if len == 0 { pos += 1; break; }
            pos += 1 + len;
        }
        pos
    };
    let question_end = qname_end + 4;

    let mut resp = Vec::with_capacity(question_end + 28);
    resp.extend_from_slice(&query[..question_end.min(query.len())]);
    while resp.len() < 12 { resp.push(0); }

    resp[2] = 0x85; resp[3] = 0x80;
    resp[4] = 0x00; resp[5] = 0x01;
    resp[6] = 0x00; resp[7] = 0x01;
    resp[8] = 0x00; resp[9] = 0x00;
    resp[10] = 0x00; resp[11] = 0x00;

    resp.extend_from_slice(&[
        0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x1E, 0x00, 0x04,
    ]);
    resp.extend_from_slice(&ip.octets());
    resp
}

fn build_nxdomain(query: &[u8]) -> Vec<u8> {
    let mut resp = query.to_vec();
    if resp.len() > 3 {
        resp[2] = 0x81;
        resp[3] = 0x83;
    }
    resp
}

/// Resolve `<svc>.<ns>` from the service map.
/// Format: "nginx.default" → parts[0]=nginx, parts[1]=default
fn resolve_from_map(
    name: &str,
    map: &std::collections::HashMap<Ipv4Addr, Vec<super::tun::ServiceEntry>>,
) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let svc_name = parts[0];
    let namespace = parts[1];

    for (ip, entries) in map {
        for entry in entries {
            if entry.name == svc_name && entry.namespace == namespace {
                return Some(*ip);
            }
        }
    }
    None
}

async fn forward_upstream(query: &[u8]) -> Option<Vec<u8>> {
    let upstream = get_upstream_dns();
    let sock = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    sock.send_to(query, &upstream).await.ok()?;
    let mut buf = vec![0u8; 4096];
    match tokio::time::timeout(std::time::Duration::from_secs(3), sock.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => { buf.truncate(n); Some(buf) }
        _ => None,
    }
}

/// Start DNS proxy. Resolves `<svc>.<ns>` from service map.
pub async fn start_dns_proxy_with_map(
    service_map: ServiceMap,
    namespaces: Vec<String>,
) -> Result<tokio::task::JoinHandle<()>> {
    let sock = UdpSocket::bind(DNS_LISTEN_ADDR)
        .await
        .with_context(|| format!("bind DNS on {DNS_LISTEN_ADDR}"))?;

    info!("DNS proxy listening on {DNS_LISTEN_ADDR}");

    let handle = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            let (n, src) = match sock.recv_from(&mut buf).await {
                Ok(x) => x,
                Err(e) => { warn!(error=%e, "DNS recv error"); continue; }
            };

            let packet = buf[..n].to_vec();
            let name = match parse_query_name(&packet) {
                Some(n) => n,
                None => continue,
            };

            debug!(name=%name, "DNS query");

            // Check if the query matches any known namespace
            // e.g. "nginx.default" → namespace = "default"
            let parts: Vec<&str> = name.split('.').collect();
            let is_cluster_query = parts.len() >= 2 && namespaces.contains(&parts[1].to_string());

            let response = if is_cluster_query {
                let map = service_map.read().await;
                match resolve_from_map(&name, &map) {
                    Some(ip) => {
                        debug!(name=%name, ip=%ip, "resolved");
                        build_dns_response(&packet, ip)
                    }
                    None => {
                        debug!(name=%name, "NXDOMAIN");
                        build_nxdomain(&packet)
                    }
                }
            } else {
                match forward_upstream(&packet).await {
                    Some(resp) => resp,
                    None => build_nxdomain(&packet),
                }
            };

            if let Err(e) = sock.send_to(&response, src).await {
                warn!(error=%e, "DNS send error");
            }
        }
    });

    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_dns_query(labels: &[&str]) -> Vec<u8> {
        let mut packet = vec![
            0x00, 0x01, // ID
            0x01, 0x00, // Flags (standard query)
            0x00, 0x01, // QDCOUNT = 1
            0x00, 0x00, // ANCOUNT = 0
            0x00, 0x00, // NSCOUNT = 0
            0x00, 0x00, // ARCOUNT = 0
        ];
        for label in labels {
            packet.push(label.len() as u8);
            packet.extend_from_slice(label.as_bytes());
        }
        packet.push(0); // root label
        packet.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // QTYPE=A, QCLASS=IN
        packet
    }

    fn make_service_map() -> HashMap<Ipv4Addr, Vec<super::super::tun::ServiceEntry>> {
        let mut map = HashMap::new();
        let ip = Ipv4Addr::new(10, 96, 1, 5);
        map.insert(
            ip,
            vec![super::super::tun::ServiceEntry {
                name: "nginx".into(),
                namespace: "default".into(),
                port: 80,
                cluster_ip: ip,
            }],
        );
        let ip2 = Ipv4Addr::new(10, 96, 1, 10);
        map.insert(
            ip2,
            vec![super::super::tun::ServiceEntry {
                name: "api".into(),
                namespace: "prod".into(),
                port: 8080,
                cluster_ip: ip2,
            }],
        );
        map
    }

    // ── parse_query_name ────────────────────────────────────

    #[test]
    fn test_parse_query_name_two_labels() {
        let packet = make_dns_query(&["nginx", "default"]);
        assert_eq!(parse_query_name(&packet).unwrap(), "nginx.default");
    }

    #[test]
    fn test_parse_query_name_three_labels() {
        let packet = make_dns_query(&["nginx", "default", "svc"]);
        assert_eq!(parse_query_name(&packet).unwrap(), "nginx.default.svc");
    }

    #[test]
    fn test_parse_query_name_single_label() {
        let packet = make_dns_query(&["localhost"]);
        assert_eq!(parse_query_name(&packet).unwrap(), "localhost");
    }

    #[test]
    fn test_parse_query_name_too_short() {
        let packet = vec![0u8; 10];
        assert!(parse_query_name(&packet).is_none());
    }

    #[test]
    fn test_parse_query_name_empty_packet() {
        let packet = vec![];
        assert!(parse_query_name(&packet).is_none());
    }

    #[test]
    fn test_parse_query_name_truncated_label() {
        // Header says label len=10 but only 3 bytes follow
        let mut packet = vec![0u8; 12];
        packet.push(10); // label length = 10
        packet.extend_from_slice(b"abc"); // only 3 bytes
        assert!(parse_query_name(&packet).is_none());
    }

    // ── build_dns_response ──────────────────────────────────

    #[test]
    fn test_build_dns_response_contains_ip() {
        let query = make_dns_query(&["nginx", "default"]);
        let ip = Ipv4Addr::new(10, 96, 1, 5);
        let resp = build_dns_response(&query, ip);

        // Response should end with the IP octets
        let len = resp.len();
        assert_eq!(&resp[len - 4..], &[10, 96, 1, 5]);
    }

    #[test]
    fn test_build_dns_response_flags() {
        let query = make_dns_query(&["nginx", "default"]);
        let resp = build_dns_response(&query, Ipv4Addr::new(1, 2, 3, 4));

        // Flags: authoritative answer, no error
        assert_eq!(resp[2], 0x85);
        assert_eq!(resp[3], 0x80);
        // QDCOUNT = 1
        assert_eq!(resp[4], 0x00);
        assert_eq!(resp[5], 0x01);
        // ANCOUNT = 1
        assert_eq!(resp[6], 0x00);
        assert_eq!(resp[7], 0x01);
    }

    #[test]
    fn test_build_dns_response_preserves_id() {
        let mut query = make_dns_query(&["test", "ns"]);
        query[0] = 0xAB;
        query[1] = 0xCD;
        let resp = build_dns_response(&query, Ipv4Addr::new(1, 1, 1, 1));
        assert_eq!(resp[0], 0xAB);
        assert_eq!(resp[1], 0xCD);
    }

    #[test]
    fn test_build_dns_response_answer_section() {
        let query = make_dns_query(&["svc", "ns"]);
        let resp = build_dns_response(&query, Ipv4Addr::new(10, 0, 0, 1));
        // Answer should contain pointer (0xC00C), type A (0x0001), class IN (0x0001)
        let answer_start = resp.len() - 16; // 12 bytes answer record + 4 bytes IP
        assert_eq!(resp[answer_start], 0xC0);
        assert_eq!(resp[answer_start + 1], 0x0C);
        assert_eq!(resp[answer_start + 2], 0x00); // TYPE A
        assert_eq!(resp[answer_start + 3], 0x01);
        assert_eq!(resp[answer_start + 4], 0x00); // CLASS IN
        assert_eq!(resp[answer_start + 5], 0x01);
    }

    // ── build_nxdomain ──────────────────────────────────────

    #[test]
    fn test_build_nxdomain_flags() {
        let query = make_dns_query(&["unknown", "ns"]);
        let resp = build_nxdomain(&query);
        assert_eq!(resp[2], 0x81);
        assert_eq!(resp[3], 0x83);
    }

    #[test]
    fn test_build_nxdomain_preserves_id() {
        let mut query = make_dns_query(&["x", "y"]);
        query[0] = 0x12;
        query[1] = 0x34;
        let resp = build_nxdomain(&query);
        assert_eq!(resp[0], 0x12);
        assert_eq!(resp[1], 0x34);
    }

    #[test]
    fn test_build_nxdomain_preserves_question() {
        let query = make_dns_query(&["test", "ns"]);
        let resp = build_nxdomain(&query);
        // Everything after flags should be preserved
        assert_eq!(resp[4..], query[4..]);
    }

    #[test]
    fn test_build_nxdomain_short_packet() {
        let resp = build_nxdomain(&[0x00, 0x01]);
        // Too short to have flags, should not panic
        assert_eq!(resp.len(), 2);
    }

    // ── resolve_from_map ────────────────────────────────────

    #[test]
    fn test_resolve_from_map_found() {
        let map = make_service_map();
        assert_eq!(
            resolve_from_map("nginx.default", &map),
            Some(Ipv4Addr::new(10, 96, 1, 5))
        );
    }

    #[test]
    fn test_resolve_from_map_different_service() {
        let map = make_service_map();
        assert_eq!(
            resolve_from_map("api.prod", &map),
            Some(Ipv4Addr::new(10, 96, 1, 10))
        );
    }

    #[test]
    fn test_resolve_from_map_unknown_service() {
        let map = make_service_map();
        assert_eq!(resolve_from_map("unknown.default", &map), None);
    }

    #[test]
    fn test_resolve_from_map_unknown_namespace() {
        let map = make_service_map();
        assert_eq!(resolve_from_map("nginx.prod", &map), None);
    }

    #[test]
    fn test_resolve_from_map_single_label() {
        let map = make_service_map();
        assert_eq!(resolve_from_map("nginx", &map), None);
    }

    #[test]
    fn test_resolve_from_map_empty_string() {
        let map = make_service_map();
        assert_eq!(resolve_from_map("", &map), None);
    }

    #[test]
    fn test_resolve_from_map_extra_labels_ignored() {
        let map = make_service_map();
        // "nginx.default.svc" → parts[0]=nginx, parts[1]=default — should match
        assert_eq!(
            resolve_from_map("nginx.default.svc", &map),
            Some(Ipv4Addr::new(10, 96, 1, 5))
        );
    }

    #[test]
    fn test_resolve_from_map_empty_map() {
        let map = HashMap::new();
        assert_eq!(resolve_from_map("nginx.default", &map), None);
    }
}
