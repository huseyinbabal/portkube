use tokio::sync::mpsc;

use crate::kube::context::{self, KubeContext};
use crate::kube::services::KubeService;
use crate::network::tun::{self, ServiceEntry, TunDevice};

// ── Events (background → main loop) ─────────────────────

pub enum AppEvent {
    ContextsLoaded { contexts: Vec<KubeContext> },
    Connected { client: kube::Client },
    ServicesLoaded { services: Vec<KubeService> },
    TunReady { device_name: String, service_cidr: String },
    ServiceMapBuilt { entries: Vec<ServiceEntry> },
    AliasAdded { ip: std::net::Ipv4Addr },
    BgHandle { handle: tokio::task::JoinHandle<()> },
    Toast { msg: String, is_error: bool },
    Error { msg: String },
}

// ── Screens & Modes ──────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    Splash,
    Contexts,
    Services,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Connecting,
}

// ── Context row ──────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ContextRow {
    pub ctx: KubeContext,
}

// ── Toast ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    pub is_error: bool,
    pub tick: u16,
}

// ── Splash state ─────────────────────────────────────────

pub struct SplashState {
    pub current_step: usize,
    pub total_steps: usize,
    pub message: String,
    pub spinner_frame: usize,
}

impl SplashState {
    pub fn new() -> Self {
        Self {
            current_step: 0,
            total_steps: 2,
            message: "Initializing...".into(),
            spinner_frame: 0,
        }
    }

    pub fn advance(&mut self, msg: &str) {
        self.current_step += 1;
        self.message = msg.into();
        self.spinner_frame = (self.spinner_frame + 1) % 10;
    }

    pub fn tick_spinner(&mut self) {
        self.spinner_frame = (self.spinner_frame + 1) % 10;
    }
}

// ── App (all state) ──────────────────────────────────────

pub struct App {
    pub screen: Screen,
    pub mode: Mode,
    pub should_quit: bool,

    // Contexts
    pub contexts: Vec<ContextRow>,
    pub ctx_selected: usize,

    // Services
    pub services: Vec<KubeService>,
    pub svc_selected: usize,

    // Connection
    pub connected_context: Option<String>,
    pub client: Option<kube::Client>,

    // Tun device
    pub tun_device: Option<TunDevice>,
    pub tun_device_name: Option<String>,
    pub service_cidr: Option<String>,
    pub service_entries: Vec<ServiceEntry>,
    pub alias_ips: Vec<std::net::Ipv4Addr>,
    pub bg_handles: Vec<tokio::task::JoinHandle<()>>,

    // Splash
    pub splash: SplashState,

    // Toast
    pub toast: Option<Toast>,
}

impl App {
    pub fn new() -> Self {
        Self {
            screen: Screen::Splash,
            mode: Mode::Normal,
            should_quit: false,
            contexts: vec![],
            ctx_selected: 0,
            services: vec![],
            svc_selected: 0,
            connected_context: None,
            client: None,
            tun_device: None,
            tun_device_name: None,
            service_cidr: None,
            service_entries: vec![],
            alias_ips: vec![],
            bg_handles: vec![],
            splash: SplashState::new(),
            toast: None,
        }
    }

    // ── Async actions ────────────────────────────────────

    pub fn connect_async(&mut self, tx: mpsc::Sender<AppEvent>) {
        if self.contexts.is_empty() {
            return;
        }
        let name = self.contexts[self.ctx_selected].ctx.name.clone();
        self.connected_context = Some(name.clone());
        self.mode = Mode::Connecting;
        self.splash = SplashState::new();
        self.splash.total_steps = 5;
        self.splash.advance(&format!("Connecting to {name}..."));

        tokio::spawn(async move {
            match context::client_for_context(&name).await {
                Ok(client) => {
                    let _ = tx.send(AppEvent::Connected { client }).await;
                }
                Err(e) => {
                    let _ = tx
                        .send(AppEvent::Error {
                            msg: format!("Connection failed: {e}"),
                        })
                        .await;
                }
            }
        });
    }

    pub fn setup_network_async(&mut self, tx: mpsc::Sender<AppEvent>) {
        let client = match &self.client {
            Some(c) => c.clone(),
            None => return,
        };
        let svcs = self.services.clone();

        tokio::spawn(async move {
            // 1. Detect service CIDR
            let cidr = tun::detect_service_cidr(&client).await;

            // 2. Create utun device + route (for general subnet awareness)
            let mut device = match tun::create_utun() {
                Ok(d) => d,
                Err(e) => {
                    // Non-fatal: proxies still work via loopback aliases
                    let _ = tx.send(AppEvent::Toast {
                        msg: format!("utun skipped: {e}"), is_error: false,
                    }).await;
                    // Continue without utun
                    let _ = tx.send(AppEvent::TunReady {
                        device_name: "none".into(), service_cidr: cidr.clone(),
                    }).await;
                    // Jump to service proxies
                    setup_proxies_and_dns(&tx, &client, &svcs, &cidr).await;
                    return;
                }
            };

            if let Err(e) = tun::configure_tun(&mut device, &cidr).await {
                let _ = tx.send(AppEvent::Toast {
                    msg: format!("utun route skipped: {e}"), is_error: false,
                }).await;
            }

            let dev_name = device.name.clone();
            let _ = tx.send(AppEvent::TunReady {
                device_name: dev_name, service_cidr: cidr.clone(),
            }).await;

            // Don't drop the device — keep fd alive
            std::mem::forget(device);

            // 3. Set up per-service proxies + DNS
            setup_proxies_and_dns(&tx, &client, &svcs, &cidr).await;
        });
    }

    // ── Sync helpers ─────────────────────────────────────

    pub fn refresh_contexts(&mut self) {
        self.contexts = context::list_contexts()
            .unwrap_or_default()
            .into_iter()
            .map(|ctx| ContextRow { ctx })
            .collect();
        self.ctx_selected = self
            .ctx_selected
            .min(self.contexts.len().saturating_sub(1));
    }

    pub fn open_in_browser(&mut self) {
        if let Some(url) = self.selected_url() {
            match open::that(&url) {
                Ok(_) => self.show_toast(&format!("Opened {url}"), false),
                Err(e) => self.show_toast(&format!("Browser error: {e}"), true),
            }
        }
    }

    pub fn copy_url(&mut self) {
        if let Some(url) = self.selected_url() {
            match arboard::Clipboard::new().and_then(|mut c| c.set_text(&url)) {
                Ok(_) => self.show_toast(&format!("Copied: {url}"), false),
                Err(e) => self.show_toast(&format!("Clipboard: {e}"), true),
            }
        }
    }

    fn selected_url(&self) -> Option<String> {
        let svc = self.services.get(self.svc_selected)?;
        let entry = self
            .service_entries
            .iter()
            .find(|e| e.name == svc.name && e.namespace == svc.namespace);
        if let Some(e) = entry {
            Some(e.url())
        } else {
            let p = svc.ports.first()?;
            let scheme = if p.port == 443 { "https" } else { "http" };
            Some(format!("{scheme}://{}.{}:{}", svc.name, svc.namespace, p.port))
        }
    }

    pub fn show_toast(&mut self, msg: &str, is_error: bool) {
        self.toast = Some(Toast {
            message: msg.to_string(),
            is_error,
            tick: 30,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube::context::KubeContext;
    use crate::kube::services::{KubeService, ServicePort as KubeServicePort};
    use std::net::Ipv4Addr;

    fn make_app_with_contexts(n: usize) -> App {
        let mut app = App::new();
        app.screen = Screen::Contexts;
        app.contexts = (0..n)
            .map(|i| ContextRow {
                ctx: KubeContext {
                    name: format!("ctx-{i}"),
                    cluster: format!("cluster-{i}"),
                    namespace: "default".into(),
                    user: format!("user-{i}"),
                    is_active: i == 0,
                },
            })
            .collect();
        app
    }

    fn make_app_with_services() -> App {
        let mut app = App::new();
        app.screen = Screen::Services;
        app.services = vec![
            KubeService {
                name: "nginx".into(),
                namespace: "default".into(),
                service_type: "ClusterIP".into(),
                cluster_ip: "10.96.0.10".into(),
                ports: vec![KubeServicePort {
                    port: 80,
                    target_port: "80".into(),
                    protocol: "TCP".into(),
                    name: None,
                }],
            },
            KubeService {
                name: "api".into(),
                namespace: "prod".into(),
                service_type: "ClusterIP".into(),
                cluster_ip: "10.96.0.20".into(),
                ports: vec![KubeServicePort {
                    port: 443,
                    target_port: "443".into(),
                    protocol: "TCP".into(),
                    name: None,
                }],
            },
        ];
        app.service_entries = vec![ServiceEntry {
            name: "nginx".into(),
            namespace: "default".into(),
            port: 80,
            target_port: Some(80),
            target_port_name: None,
            cluster_ip: Ipv4Addr::new(10, 96, 0, 10),
        }];
        app
    }

    // ── App::new ────────────────────────────────────────────

    #[test]
    fn test_app_new_defaults() {
        let app = App::new();
        assert_eq!(app.screen, Screen::Splash);
        assert_eq!(app.mode, Mode::Normal);
        assert!(!app.should_quit);
        assert!(app.contexts.is_empty());
        assert_eq!(app.ctx_selected, 0);
        assert!(app.services.is_empty());
        assert_eq!(app.svc_selected, 0);
        assert!(app.connected_context.is_none());
        assert!(app.client.is_none());
        assert!(app.tun_device.is_none());
        assert!(app.tun_device_name.is_none());
        assert!(app.service_cidr.is_none());
        assert!(app.service_entries.is_empty());
        assert!(app.alias_ips.is_empty());
        assert!(app.bg_handles.is_empty());
        assert!(app.toast.is_none());
    }

    // ── SplashState ─────────────────────────────────────────

    #[test]
    fn test_splash_state_new() {
        let s = SplashState::new();
        assert_eq!(s.current_step, 0);
        assert_eq!(s.total_steps, 2);
        assert_eq!(s.message, "Initializing...");
        assert_eq!(s.spinner_frame, 0);
    }

    #[test]
    fn test_splash_state_advance() {
        let mut s = SplashState::new();
        s.advance("Loading...");
        assert_eq!(s.current_step, 1);
        assert_eq!(s.message, "Loading...");
        assert_eq!(s.spinner_frame, 1);
    }

    #[test]
    fn test_splash_state_advance_multiple() {
        let mut s = SplashState::new();
        s.advance("Step 1");
        s.advance("Step 2");
        assert_eq!(s.current_step, 2);
        assert_eq!(s.message, "Step 2");
    }

    #[test]
    fn test_splash_state_tick_spinner() {
        let mut s = SplashState::new();
        s.tick_spinner();
        assert_eq!(s.spinner_frame, 1);
        for _ in 0..9 {
            s.tick_spinner();
        }
        assert_eq!(s.spinner_frame, 0); // wraps at 10
    }

    // ── Toast ───────────────────────────────────────────────

    #[test]
    fn test_show_toast() {
        let mut app = App::new();
        app.show_toast("Hello", false);
        let toast = app.toast.as_ref().unwrap();
        assert_eq!(toast.message, "Hello");
        assert!(!toast.is_error);
        assert_eq!(toast.tick, 30);
    }

    #[test]
    fn test_show_toast_error() {
        let mut app = App::new();
        app.show_toast("Oops", true);
        let toast = app.toast.as_ref().unwrap();
        assert!(toast.is_error);
    }

    #[test]
    fn test_show_toast_replaces_previous() {
        let mut app = App::new();
        app.show_toast("First", false);
        app.show_toast("Second", false);
        assert_eq!(app.toast.as_ref().unwrap().message, "Second");
    }

    // ── selected_url ────────────────────────────────────────

    #[test]
    fn test_selected_url_with_matching_entry() {
        let app = make_app_with_services();
        // svc_selected=0 → nginx, which has an entry
        let url = app.selected_url().unwrap();
        assert_eq!(url, "http://nginx.default");
    }

    #[test]
    fn test_selected_url_without_matching_entry() {
        let mut app = make_app_with_services();
        app.svc_selected = 1; // api.prod — no entry in service_entries
        let url = app.selected_url().unwrap();
        assert_eq!(url, "https://api.prod:443");
    }

    #[test]
    fn test_selected_url_empty_services() {
        let app = App::new();
        assert!(app.selected_url().is_none());
    }

    #[test]
    fn test_selected_url_out_of_bounds() {
        let mut app = make_app_with_services();
        app.svc_selected = 99;
        assert!(app.selected_url().is_none());
    }

    // ── connect_async guards ────────────────────────────────

    #[tokio::test]
    async fn test_connect_async_empty_contexts_noop() {
        let mut app = App::new();
        let (tx, _rx) = mpsc::channel(10);
        app.connect_async(tx);
        // Should not change mode when no contexts
        assert_eq!(app.mode, Mode::Normal);
    }

    #[tokio::test]
    async fn test_connect_async_sets_connecting_mode() {
        let mut app = make_app_with_contexts(2);
        app.ctx_selected = 1;
        let (tx, _rx) = mpsc::channel(10);
        app.connect_async(tx);
        assert_eq!(app.mode, Mode::Connecting);
        assert_eq!(app.connected_context, Some("ctx-1".into()));
    }

    // ── Screen/Mode enums ───────────────────────────────────

    #[test]
    fn test_screen_eq() {
        assert_eq!(Screen::Splash, Screen::Splash);
        assert_ne!(Screen::Splash, Screen::Contexts);
        assert_ne!(Screen::Contexts, Screen::Services);
    }

    #[test]
    fn test_mode_eq() {
        assert_eq!(Mode::Normal, Mode::Normal);
        assert_ne!(Mode::Normal, Mode::Connecting);
    }
}

/// Set up per-service TCP proxies (loopback alias + portforward) and DNS.
/// Called from inside a tokio::spawn.
async fn setup_proxies_and_dns(
    tx: &mpsc::Sender<AppEvent>,
    client: &kube::Client,
    svcs: &[KubeService],
    _cidr: &str,
) {
    // 1. Build service map for DNS
    let svc_map = tun::build_service_map(svcs);
    let entries: Vec<ServiceEntry> = svc_map.read().await.values().flatten().cloned().collect();
    let _ = tx.send(AppEvent::ServiceMapBuilt { entries }).await;

    // 2. Start a TCP proxy for each service (loopback alias + kube-rs portforward)
    let mut proxy_count = 0u32;
    let all_entries: Vec<ServiceEntry> = svc_map.read().await.values().flatten().cloned().collect();

    // Skip system namespaces — proxying kube-dns etc breaks networking
    let skip_ns = ["kube-system", "kube-public", "kube-node-lease"];

    for entry in &all_entries {
        if skip_ns.contains(&entry.namespace.as_str()) {
            continue;
        }
        match tun::start_service_proxy(client, entry).await {
            Ok((handle, ip)) => {
                let _ = tx.send(AppEvent::BgHandle { handle }).await;
                let _ = tx.send(AppEvent::AliasAdded { ip }).await;
                proxy_count += 1;
            }
            Err(e) => {
                let _ = tx.send(AppEvent::Toast {
                    msg: format!("skip {}: {e}", entry.name),
                    is_error: false,
                }).await;
            }
        }
    }

    // 3. Collect unique namespaces (excluding system ones)
    let namespaces: Vec<String> = {
        let mut ns: Vec<String> = all_entries.iter()
            .filter(|e| !skip_ns.contains(&e.namespace.as_str()))
            .map(|e| e.namespace.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        ns.sort();
        ns
    };

    // 4. Install /etc/resolver/<namespace> for each namespace
    if let Err(e) = crate::network::resolver::install(&namespaces).await {
        let _ = tx.send(AppEvent::Error {
            msg: format!("Resolver failed: {e}"),
        }).await;
        return;
    }

    // 5. DNS proxy — resolves <svc>.<ns> from service map
    match crate::network::dns::start_dns_proxy_with_map(svc_map, namespaces).await {
        Ok(handle) => {
            let _ = tx.send(AppEvent::BgHandle { handle }).await;
        }
        Err(e) => {
            let _ = tx.send(AppEvent::Error {
                msg: format!("DNS proxy failed: {e}"),
            }).await;
            return;
        }
    }

    let _ = tx.send(AppEvent::Toast {
        msg: format!("Ready — {proxy_count} service(s), use <svc>.<ns> in browser"),
        is_error: false,
    }).await;
}
