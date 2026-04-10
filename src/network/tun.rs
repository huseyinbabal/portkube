//! Network data plane: TUN device for routing + loopback alias + TCP portforward.
//!
//! Architecture:
//!   1. TUN device + route → OS knows about cluster subnet (10.96.0.0/12)
//!   2. DNS proxy → resolves *.svc.cluster.local → ClusterIP
//!   3. For each known service: loopback alias on ClusterIP + TCP listener
//!   4. TCP listener → kube-rs portforward (WebSocket) → pod
//!
//! Supports macOS (utun), Linux (tun), and Windows (no TUN, proxies only).

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::{api::ListParams, Api};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

#[cfg(unix)]
use std::os::unix::io::RawFd;

const TUN_LOCAL_IP: &str = "198.18.0.1";
const TUN_PEER_IP: &str = "198.18.0.2";

// ── Types ────────────────────────────────────────────────

pub type ServiceMap = Arc<RwLock<HashMap<Ipv4Addr, Vec<ServiceEntry>>>>;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ServiceEntry {
    pub name: String,
    pub namespace: String,
    pub port: u16,
    pub cluster_ip: Ipv4Addr,
}

impl ServiceEntry {
    /// Short DNS name: `nginx.default`
    pub fn dns_name(&self) -> String {
        format!("{}.{}", self.name, self.namespace)
    }

    pub fn url(&self) -> String {
        let scheme = if self.port == 443 { "https" } else { "http" };
        if self.port == 80 || self.port == 443 {
            format!("{scheme}://{}", self.dns_name())
        } else {
            format!("{scheme}://{}:{}", self.dns_name(), self.port)
        }
    }
}

// ── TUN device ──────────────────────────────────────────

#[cfg(unix)]
pub struct TunDevice {
    pub name: String,
    pub fd: RawFd,
    pub service_cidr: String,
}

#[cfg(windows)]
pub struct TunDevice {
    pub name: String,
    pub service_cidr: String,
}

#[cfg(unix)]
impl Drop for TunDevice {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd); }
    }
}

// ── macOS: utun via PF_SYSTEM ───────────────────────────

#[cfg(target_os = "macos")]
pub fn create_utun() -> Result<TunDevice> {
    let fd = unsafe {
        libc::socket(32 /* PF_SYSTEM */, libc::SOCK_DGRAM, 2 /* SYSPROTO_CONTROL */)
    };
    if fd < 0 {
        anyhow::bail!("socket(PF_SYSTEM) failed: {} — run with sudo", std::io::Error::last_os_error());
    }

    #[repr(C)]
    struct CtlInfo { ctl_id: u32, ctl_name: [u8; 96] }

    let mut info = CtlInfo { ctl_id: 0, ctl_name: [0u8; 96] };
    let name = b"com.apple.net.utun_control";
    info.ctl_name[..name.len()].copy_from_slice(name);

    let ret = unsafe { libc::ioctl(fd, 0xC064_4E03u64 as libc::c_ulong, &mut info) };
    if ret < 0 {
        unsafe { libc::close(fd); }
        anyhow::bail!("ioctl(CTLIOCGINFO) failed: {}", std::io::Error::last_os_error());
    }

    #[repr(C)]
    struct SockaddrCtl {
        sc_len: u8, sc_family: u8, ss_sysaddr: u16,
        sc_id: u32, sc_unit: u32, sc_reserved: [u32; 5],
    }

    let addr = SockaddrCtl {
        sc_len: std::mem::size_of::<SockaddrCtl>() as u8,
        sc_family: 32, ss_sysaddr: 2,
        sc_id: info.ctl_id, sc_unit: 0, sc_reserved: [0; 5],
    };

    let ret = unsafe {
        libc::connect(fd, &addr as *const SockaddrCtl as *const libc::sockaddr,
            std::mem::size_of::<SockaddrCtl>() as libc::socklen_t)
    };
    if ret < 0 {
        unsafe { libc::close(fd); }
        anyhow::bail!("connect(utun) failed: {}", std::io::Error::last_os_error());
    }

    let mut ifname_buf = [0u8; 32];
    let mut ifname_len: libc::socklen_t = ifname_buf.len() as u32;
    let ret = unsafe {
        libc::getsockopt(fd, 2, 2, ifname_buf.as_mut_ptr() as *mut libc::c_void, &mut ifname_len)
    };
    if ret < 0 {
        unsafe { libc::close(fd); }
        anyhow::bail!("getsockopt(UTUN_OPT_IFNAME) failed: {}", std::io::Error::last_os_error());
    }

    let name = std::str::from_utf8(&ifname_buf[..ifname_len as usize])
        .unwrap_or("utun?").trim_end_matches('\0').to_string();

    info!(device=%name, "utun device created");
    Ok(TunDevice { name, fd, service_cidr: String::new() })
}

// ── Linux: TUN via /dev/net/tun ─────────────────────────

#[cfg(target_os = "linux")]
pub fn create_utun() -> Result<TunDevice> {
    use std::os::unix::io::IntoRawFd;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/net/tun")
        .context("open /dev/net/tun — run with sudo or load tun kernel module")?;
    let fd = file.into_raw_fd();

    #[repr(C)]
    struct Ifreq {
        ifr_name: [u8; 16],
        ifr_flags: i16,
        _pad: [u8; 22],
    }

    const IFF_TUN: i16 = 0x0001;
    const IFF_NO_PI: i16 = 0x1000;
    const TUNSETIFF: u64 = 0x400454CA;

    let mut ifr = Ifreq {
        ifr_name: [0u8; 16],
        ifr_flags: IFF_TUN | IFF_NO_PI,
        _pad: [0u8; 22],
    };
    let prefix = b"portkube";
    ifr.ifr_name[..prefix.len()].copy_from_slice(prefix);

    let ret = unsafe { libc::ioctl(fd, TUNSETIFF as _, &mut ifr) };
    if ret < 0 {
        unsafe { libc::close(fd); }
        anyhow::bail!("ioctl(TUNSETIFF) failed: {}", std::io::Error::last_os_error());
    }

    let name_len = ifr.ifr_name.iter().position(|&b| b == 0).unwrap_or(16);
    let name = std::str::from_utf8(&ifr.ifr_name[..name_len])
        .unwrap_or("portkube0").to_string();

    info!(device=%name, "tun device created");
    Ok(TunDevice { name, fd, service_cidr: String::new() })
}

// ── Windows: no TUN support ─────────────────────────────

#[cfg(windows)]
pub fn create_utun() -> Result<TunDevice> {
    anyhow::bail!("TUN devices are not yet supported on Windows — proxies will still work via loopback")
}

// ── Configure TUN ───────────────────────────────────────

#[cfg(target_os = "macos")]
pub async fn configure_tun(tun: &mut TunDevice, service_cidr: &str) -> Result<()> {
    let dev = &tun.name;
    run_cmd("ifconfig", &[dev, "inet", TUN_LOCAL_IP, TUN_PEER_IP, "up"]).await?;
    run_cmd("route", &["add", "-net", service_cidr, TUN_PEER_IP]).await?;
    tun.service_cidr = service_cidr.to_string();
    info!(device=%dev, cidr=%service_cidr, "tun configured");
    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn configure_tun(tun: &mut TunDevice, service_cidr: &str) -> Result<()> {
    let dev = &tun.name;
    run_cmd("ip", &["addr", "add", &format!("{TUN_LOCAL_IP}/32"), "peer", TUN_PEER_IP, "dev", dev]).await?;
    run_cmd("ip", &["link", "set", dev, "up"]).await?;
    run_cmd("ip", &["route", "add", service_cidr, "via", TUN_PEER_IP, "dev", dev]).await?;
    tun.service_cidr = service_cidr.to_string();
    info!(device=%dev, cidr=%service_cidr, "tun configured");
    Ok(())
}

#[cfg(windows)]
pub async fn configure_tun(tun: &mut TunDevice, service_cidr: &str) -> Result<()> {
    tun.service_cidr = service_cidr.to_string();
    anyhow::bail!("TUN configuration is not supported on Windows")
}

// ── Cleanup TUN ─────────────────────────────────────────

#[cfg(target_os = "macos")]
pub async fn cleanup_tun(tun: &TunDevice) {
    if !tun.service_cidr.is_empty() {
        let _ = run_cmd("route", &["delete", "-net", &tun.service_cidr, TUN_PEER_IP]).await;
    }
    info!(device=%tun.name, "tun cleaned up");
}

#[cfg(target_os = "linux")]
pub async fn cleanup_tun(tun: &TunDevice) {
    if !tun.service_cidr.is_empty() {
        let _ = run_cmd("ip", &["route", "del", &tun.service_cidr, "via", TUN_PEER_IP]).await;
    }
    info!(device=%tun.name, "tun cleaned up");
}

#[cfg(windows)]
pub async fn cleanup_tun(tun: &TunDevice) {
    info!(device=%tun.name, "tun cleanup (no-op on Windows)");
}

// ── Loopback alias per ClusterIP ─────────────────────────

#[cfg(target_os = "macos")]
async fn add_loopback_alias(ip: Ipv4Addr) -> Result<()> {
    let ip_str = ip.to_string();
    run_cmd("ifconfig", &["lo0", "alias", &ip_str]).await?;
    debug!(ip=%ip, "loopback alias added");
    Ok(())
}

#[cfg(target_os = "linux")]
async fn add_loopback_alias(ip: Ipv4Addr) -> Result<()> {
    let ip_cidr = format!("{ip}/32");
    run_cmd("ip", &["addr", "add", &ip_cidr, "dev", "lo"]).await?;
    debug!(ip=%ip, "loopback alias added");
    Ok(())
}

#[cfg(windows)]
async fn add_loopback_alias(ip: Ipv4Addr) -> Result<()> {
    let ip_str = ip.to_string();
    run_cmd("netsh", &["interface", "ip", "add", "address", "Loopback Pseudo-Interface 1", &ip_str, "255.255.255.255"]).await?;
    debug!(ip=%ip, "loopback alias added");
    Ok(())
}

#[cfg(target_os = "macos")]
async fn remove_loopback_alias(ip: Ipv4Addr) {
    let ip_str = ip.to_string();
    let _ = run_cmd("ifconfig", &["lo0", "-alias", &ip_str]).await;
    debug!(ip=%ip, "loopback alias removed");
}

#[cfg(target_os = "linux")]
async fn remove_loopback_alias(ip: Ipv4Addr) {
    let ip_cidr = format!("{ip}/32");
    let _ = run_cmd("ip", &["addr", "del", &ip_cidr, "dev", "lo"]).await;
    debug!(ip=%ip, "loopback alias removed");
}

#[cfg(windows)]
async fn remove_loopback_alias(ip: Ipv4Addr) {
    let ip_str = ip.to_string();
    let _ = run_cmd("netsh", &["interface", "ip", "delete", "address", "Loopback Pseudo-Interface 1", &ip_str]).await;
    debug!(ip=%ip, "loopback alias removed");
}

/// Remove all loopback aliases.
pub async fn cleanup_aliases(ips: &[Ipv4Addr]) {
    for ip in ips {
        remove_loopback_alias(*ip).await;
    }
    if !ips.is_empty() {
        info!(count=ips.len(), "loopback aliases cleaned up");
    }
}

// ── Pod resolution ───────────────────────────────────────

/// Find a ready pod backing a service via its label selector.
async fn resolve_pod(client: &kube::Client, ns: &str, svc_name: &str) -> Result<String> {
    let svc_api: Api<k8s_openapi::api::core::v1::Service> =
        Api::namespaced(client.clone(), ns);
    let svc = svc_api.get(svc_name).await.context("get service")?;

    let selector = svc.spec.and_then(|s| s.selector).unwrap_or_default();
    if selector.is_empty() {
        anyhow::bail!("service {svc_name} has no selector");
    }

    let label_sel = selector.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");

    let pod_api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let pods = pod_api.list(&ListParams::default().labels(&label_sel)).await?;

    for pod in &pods.items {
        if let Some(status) = &pod.status {
            let ready = status.conditions.as_ref()
                .map(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
                .unwrap_or(false);
            if ready {
                if let Some(name) = &pod.metadata.name {
                    return Ok(name.clone());
                }
            }
        }
    }
    anyhow::bail!("no ready pods for service {svc_name}")
}

// ── TCP portforward listener ─────────────────────────────

/// Start a TCP listener on `cluster_ip:port` that proxies each connection
/// via kube-rs portforward to the backing pod. Returns a JoinHandle.
pub async fn start_service_proxy(
    client: &kube::Client,
    entry: &ServiceEntry,
) -> Result<(tokio::task::JoinHandle<()>, Ipv4Addr)> {
    let ip = entry.cluster_ip;
    let port = entry.port;
    let ns = entry.namespace.clone();
    let svc = entry.name.clone();

    // Add loopback alias so we can bind on this ClusterIP
    add_loopback_alias(ip).await?;

    // Resolve backing pod
    let pod_name = resolve_pod(client, &ns, &svc).await?;
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), &ns);

    let bind_addr = format!("{ip}:{port}");
    let listener = TcpListener::bind(&bind_addr).await
        .with_context(|| format!("bind {bind_addr}"))?;

    info!(addr=%bind_addr, svc=%svc, pod=%pod_name, "proxy listening");

    let handle = tokio::spawn(async move {
        loop {
            let (mut tcp, _) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => { warn!(error=%e, "accept error"); continue; }
            };

            let pod_api = pod_api.clone();
            let pod_name = pod_name.clone();

            tokio::spawn(async move {
                let mut pf = match pod_api.portforward(&pod_name, &[port]).await {
                    Ok(pf) => pf,
                    Err(e) => { error!(error=%e, "portforward failed"); return; }
                };
                if let Some(mut upstream) = pf.take_stream(port) {
                    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut upstream).await;
                }
            });
        }
    });

    Ok((handle, ip))
}

// ── Service CIDR detection ───────────────────────────────

/// Parse a CIDR from a k8s API error message like "...CIDR 10.96.0.0/12..."
pub fn parse_cidr_from_error(msg: &str) -> Option<String> {
    let idx = msg.find("CIDR ")?;
    let rest = &msg[idx + 5..];
    let cidr = rest
        .split(|c: char| !c.is_ascii_digit() && c != '.' && c != '/')
        .next()
        .unwrap_or("");
    if cidr.contains('/') {
        Some(cidr.to_string())
    } else {
        None
    }
}

/// Detect the cluster service CIDR by creating an invalid service.
pub async fn detect_service_cidr(client: &kube::Client) -> String {
    use k8s_openapi::api::core::v1::Service;
    use kube::Api;

    let api: Api<Service> = Api::namespaced(client.clone(), "default");

    let dummy: serde_json::Value = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": { "name": "portkube-cidr-probe", "namespace": "default" },
        "spec": {
            "clusterIP": "1.1.1.1",
            "ports": [{ "port": 443, "protocol": "TCP" }]
        }
    });

    match api.create(
        &kube::api::PostParams::default(),
        &serde_json::from_value::<Service>(dummy).unwrap(),
    ).await {
        Err(kube::Error::Api(resp)) => {
            if let Some(cidr) = parse_cidr_from_error(&resp.message) {
                info!(cidr=%cidr, "detected service CIDR");
                return cidr;
            }
        }
        Ok(_) => {
            let _ = api.delete("portkube-cidr-probe", &Default::default()).await;
        }
        _ => {}
    }

    warn!("could not detect service CIDR, using default 10.96.0.0/12");
    "10.96.0.0/12".to_string()
}

// ── Service map builder ──────────────────────────────────

pub fn build_service_map(services: &[crate::kube::services::KubeService]) -> ServiceMap {
    let mut map: HashMap<Ipv4Addr, Vec<ServiceEntry>> = HashMap::new();
    for svc in services {
        let ip: Ipv4Addr = match svc.cluster_ip.parse::<Ipv4Addr>() {
            Ok(ip) if !ip.is_loopback() && !ip.is_unspecified() => ip,
            _ => continue,
        };
        for sp in &svc.ports {
            map.entry(ip).or_default().push(ServiceEntry {
                name: svc.name.clone(),
                namespace: svc.namespace.clone(),
                port: sp.port as u16,
                cluster_ip: ip,
            });
        }
    }
    Arc::new(RwLock::new(map))
}

// ── Helpers ──────────────────────────────────────────────

async fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .with_context(|| format!("{cmd} {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{cmd} {} failed (exit {:?}): {}",
            args.join(" "), output.status.code(), stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube::services::{KubeService, ServicePort};

    fn entry(name: &str, ns: &str, port: u16, ip: [u8; 4]) -> ServiceEntry {
        ServiceEntry {
            name: name.into(),
            namespace: ns.into(),
            port,
            cluster_ip: Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]),
        }
    }

    fn kube_svc(name: &str, ns: &str, ip: &str, ports: Vec<i32>) -> KubeService {
        KubeService {
            name: name.into(),
            namespace: ns.into(),
            service_type: "ClusterIP".into(),
            cluster_ip: ip.into(),
            ports: ports
                .into_iter()
                .map(|p| ServicePort {
                    port: p,
                    target_port: p.to_string(),
                    protocol: "TCP".into(),
                    name: None,
                })
                .collect(),
        }
    }

    // ── ServiceEntry ────────────────────────────────────────

    #[test]
    fn test_service_entry_dns_name() {
        let e = entry("nginx", "default", 80, [10, 96, 1, 1]);
        assert_eq!(e.dns_name(), "nginx.default");
    }

    #[test]
    fn test_service_entry_url_http_port_80() {
        let e = entry("nginx", "default", 80, [10, 96, 1, 1]);
        assert_eq!(e.url(), "http://nginx.default");
    }

    #[test]
    fn test_service_entry_url_https_port_443() {
        let e = entry("api", "prod", 443, [10, 96, 1, 2]);
        assert_eq!(e.url(), "https://api.prod");
    }

    #[test]
    fn test_service_entry_url_custom_port() {
        let e = entry("grafana", "monitoring", 3000, [10, 96, 1, 5]);
        assert_eq!(e.url(), "http://grafana.monitoring:3000");
    }

    #[test]
    fn test_service_entry_url_non_standard_https() {
        let e = entry("api", "prod", 8443, [10, 96, 1, 3]);
        assert_eq!(e.url(), "http://api.prod:8443");
    }

    // ── build_service_map ───────────────────────────────────

    #[tokio::test]
    async fn test_build_service_map_basic() {
        let svcs = vec![kube_svc("nginx", "default", "10.96.0.10", vec![80])];
        let map = build_service_map(&svcs);
        let read = map.read().await;
        let ip = Ipv4Addr::new(10, 96, 0, 10);
        assert!(read.contains_key(&ip));
        assert_eq!(read[&ip].len(), 1);
        assert_eq!(read[&ip][0].name, "nginx");
        assert_eq!(read[&ip][0].port, 80);
    }

    #[tokio::test]
    async fn test_build_service_map_multiple_ports() {
        let svcs = vec![kube_svc("api", "default", "10.96.0.11", vec![80, 443])];
        let map = build_service_map(&svcs);
        let read = map.read().await;
        let ip = Ipv4Addr::new(10, 96, 0, 11);
        assert_eq!(read[&ip].len(), 2);
        assert_eq!(read[&ip][0].port, 80);
        assert_eq!(read[&ip][1].port, 443);
    }

    #[tokio::test]
    async fn test_build_service_map_filters_loopback() {
        let svcs = vec![kube_svc("local", "default", "127.0.0.1", vec![80])];
        let map = build_service_map(&svcs);
        let read = map.read().await;
        assert!(read.is_empty());
    }

    #[tokio::test]
    async fn test_build_service_map_filters_unspecified() {
        let svcs = vec![kube_svc("none", "default", "0.0.0.0", vec![80])];
        let map = build_service_map(&svcs);
        let read = map.read().await;
        assert!(read.is_empty());
    }

    #[tokio::test]
    async fn test_build_service_map_filters_invalid_ip() {
        let svcs = vec![kube_svc("bad", "default", "None", vec![80])];
        let map = build_service_map(&svcs);
        let read = map.read().await;
        assert!(read.is_empty());
    }

    #[tokio::test]
    async fn test_build_service_map_filters_headless() {
        let svcs = vec![kube_svc("headless", "default", "-", vec![80])];
        let map = build_service_map(&svcs);
        let read = map.read().await;
        assert!(read.is_empty());
    }

    #[tokio::test]
    async fn test_build_service_map_empty_services() {
        let svcs: Vec<KubeService> = vec![];
        let map = build_service_map(&svcs);
        let read = map.read().await;
        assert!(read.is_empty());
    }

    #[tokio::test]
    async fn test_build_service_map_multiple_services() {
        let svcs = vec![
            kube_svc("nginx", "default", "10.96.0.10", vec![80]),
            kube_svc("api", "prod", "10.96.0.20", vec![8080]),
        ];
        let map = build_service_map(&svcs);
        let read = map.read().await;
        assert_eq!(read.len(), 2);
    }

    #[tokio::test]
    async fn test_build_service_map_no_ports() {
        let svcs = vec![kube_svc("empty", "default", "10.96.0.10", vec![])];
        let map = build_service_map(&svcs);
        let read = map.read().await;
        assert!(read.is_empty());
    }

    // ── parse_cidr_from_error ───────────────────────────────

    #[test]
    fn test_parse_cidr_standard_message() {
        let msg = "Service \"portkube-cidr-probe\" is invalid: spec.clusterIPs: Invalid value: []string{\"1.1.1.1\"}: failed to allocate an IP. The range of valid IPs is CIDR 10.96.0.0/12";
        assert_eq!(parse_cidr_from_error(msg), Some("10.96.0.0/12".into()));
    }

    #[test]
    fn test_parse_cidr_different_range() {
        let msg = "... valid IPs is CIDR 172.20.0.0/16 ...";
        assert_eq!(parse_cidr_from_error(msg), Some("172.20.0.0/16".into()));
    }

    #[test]
    fn test_parse_cidr_no_cidr_keyword() {
        let msg = "some random error without cidr info";
        assert_eq!(parse_cidr_from_error(msg), None);
    }

    #[test]
    fn test_parse_cidr_cidr_but_no_slash() {
        let msg = "CIDR 10.96.0.0";
        assert_eq!(parse_cidr_from_error(msg), None);
    }

    #[test]
    fn test_parse_cidr_empty_message() {
        assert_eq!(parse_cidr_from_error(""), None);
    }

    // ── detect_service_cidr with fake k8s ───────────────────

    use crate::test_utils::*;

    #[tokio::test]
    async fn test_detect_cidr_from_api_error() {
        let client = fake_k8s(|_uri, method| {
            if *method == http::Method::POST {
                (422, error_status_json(422,
                    "Service is invalid: ... The range of valid IPs is CIDR 10.96.0.0/12"))
            } else {
                (200, serde_json::json!({}))
            }
        });
        let cidr = detect_service_cidr(&client).await;
        assert_eq!(cidr, "10.96.0.0/12");
    }

    #[tokio::test]
    async fn test_detect_cidr_fallback_when_no_cidr_in_error() {
        let client = fake_k8s(|_uri, method| {
            if *method == http::Method::POST {
                (422, error_status_json(422, "Something went wrong"))
            } else {
                (200, serde_json::json!({}))
            }
        });
        let cidr = detect_service_cidr(&client).await;
        assert_eq!(cidr, "10.96.0.0/12"); // fallback default
    }

    #[tokio::test]
    async fn test_detect_cidr_fallback_on_non_api_error() {
        let client = fake_k8s(|_uri, _method| {
            (500, serde_json::json!({"error": "internal server error"}))
        });
        let cidr = detect_service_cidr(&client).await;
        assert_eq!(cidr, "10.96.0.0/12"); // fallback default
    }
}
