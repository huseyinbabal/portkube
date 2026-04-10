mod kube;
mod network;
#[cfg(test)]
mod test_utils;
mod tui;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use tui::app::{App, AppEvent, Mode, Screen};
use tui::ui;

#[tokio::main]
async fn main() -> Result<()> {
    // Check root/admin
    if !is_elevated() {
        eprintln!("portkube requires root/admin privileges for network tunneling.");
        #[cfg(unix)]
        eprintln!("Run with: sudo portkube");
        #[cfg(windows)]
        eprintln!("Run as Administrator.");
        std::process::exit(1);
    }

    // Clean up any leftovers from a previous crash
    let _ = network::resolver::uninstall().await;

    let (tx, mut rx) = mpsc::channel::<AppEvent>(100);
    let mut app = App::new();

    // Load contexts in background so splash shows immediately
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            // Small delay so user sees the splash
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = tx.send(AppEvent::Toast { msg: "Loading kubeconfig...".into(), is_error: false }).await;
            tokio::time::sleep(Duration::from_millis(300)).await;

            let contexts = crate::kube::context::list_contexts().unwrap_or_default();
            let _ = tx.send(AppEvent::ContextsLoaded { contexts }).await;
        });
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(100);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| ui::render(f, &app))?;

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    app.should_quit = true;
                } else if app.screen == Screen::Splash {
                    // Ignore keys during splash — auto-transitions when loaded
                } else {
                    match app.mode {
                        Mode::Normal => handle_normal_key(&mut app, key.code, &tx),
                        Mode::Connecting => {
                            if key.code == KeyCode::Char('q') || key.code == KeyCode::Esc {
                                app.should_quit = true;
                            }
                        }
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
            if let Some(t) = &mut app.toast {
                if t.tick == 0 {
                    app.toast = None;
                } else {
                    t.tick = t.tick.saturating_sub(1);
                }
            }
            // Advance splash spinner
            if app.screen == Screen::Splash || app.mode == Mode::Connecting {
                app.splash.tick_spinner();
            }
        }

        while let Ok(ev) = rx.try_recv() {
            process_event(&mut app, ev, &tx);
        }

        if app.should_quit {
            break;
        }
    }

    // Cleanup
    for h in app.bg_handles.drain(..) {
        h.abort();
    }
    network::tun::cleanup_aliases(&app.alias_ips).await;
    if let Some(tun) = &app.tun_device {
        network::tun::cleanup_tun(tun).await;
    }
    let _ = network::resolver::uninstall().await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn process_event(app: &mut App, ev: AppEvent, tx: &mpsc::Sender<AppEvent>) {
    match ev {
        AppEvent::ContextsLoaded { contexts } => {
            app.contexts = contexts
                .into_iter()
                .map(|ctx| tui::app::ContextRow { ctx })
                .collect();
            app.splash.advance(&format!(
                "{} contexts loaded",
                app.contexts.len()
            ));
            app.screen = Screen::Contexts;
        }
        AppEvent::Connected { client } => {
            app.client = Some(client);
            app.splash.advance("Discovering services...");
            let tx2 = tx.clone();
            let c = app.client.clone().unwrap();
            tokio::spawn(async move {
                match crate::kube::services::list_services(&c, None).await {
                    Ok(svcs) => {
                        let _ =
                            tx2.send(AppEvent::ServicesLoaded { services: svcs }).await;
                    }
                    Err(e) => {
                        let _ = tx2
                            .send(AppEvent::Error {
                                msg: format!("Service listing failed: {e}"),
                            })
                            .await;
                    }
                }
            });
        }
        AppEvent::ServicesLoaded { services } => {
            app.services = services;
            app.svc_selected = 0;
            app.splash.advance(&format!("{} services found — setting up network...", app.services.len()));
            app.setup_network_async(tx.clone());
        }
        AppEvent::TunReady {
            device_name,
            service_cidr,
        } => {
            app.tun_device_name = Some(device_name.clone());
            app.service_cidr = Some(service_cidr);
            app.splash.advance(&format!("Network device {device_name} ready"));
        }
        AppEvent::ServiceMapBuilt { entries } => {
            app.service_entries = entries;
            app.splash.advance("Starting service proxies...");
        }
        AppEvent::AliasAdded { ip } => {
            app.alias_ips.push(ip);
        }
        AppEvent::BgHandle { handle } => {
            app.bg_handles.push(handle);
        }
        AppEvent::Toast { msg, is_error } => {
            if app.mode == Mode::Connecting && !is_error {
                app.splash.advance(&msg);
                if msg.contains("Ready") || msg.contains("proxied") || msg.contains("use <svc>") {
                    app.mode = Mode::Normal;
                    app.screen = Screen::Services;
                    app.splash = tui::app::SplashState::new();
                    app.show_toast(&msg, false);
                }
            } else {
                app.show_toast(&msg, is_error);
            }
        }
        AppEvent::Error { msg } => {
            app.show_toast(&msg, true);
            app.mode = Mode::Normal;
            app.splash = tui::app::SplashState::new();
        }
    }
}

fn handle_normal_key(app: &mut App, key: KeyCode, tx: &mpsc::Sender<AppEvent>) {
    match app.screen {
        Screen::Splash => {} // auto-transitions, no key handling
        Screen::Contexts => handle_ctx_key(app, key, tx),
        Screen::Services => handle_svc_key(app, key, tx),
    }
}

fn handle_ctx_key(app: &mut App, key: KeyCode, tx: &mpsc::Sender<AppEvent>) {
    match key {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Up | KeyCode::Char('k') => {
            app.ctx_selected = app.ctx_selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.ctx_selected + 1 < app.contexts.len() {
                app.ctx_selected += 1;
            }
        }
        KeyCode::Home | KeyCode::Char('g') => app.ctx_selected = 0,
        KeyCode::End | KeyCode::Char('G') => {
            app.ctx_selected = app.contexts.len().saturating_sub(1);
        }
        KeyCode::Char('r') => {
            app.refresh_contexts();
            app.show_toast("Refreshed", false);
        }
        KeyCode::Enter => {
            app.connect_async(tx.clone());
        }
        _ => {}
    }
}

fn handle_svc_key(app: &mut App, key: KeyCode, tx: &mpsc::Sender<AppEvent>) {
    match key {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Up | KeyCode::Char('k') => {
            app.svc_selected = app.svc_selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.svc_selected + 1 < app.services.len() {
                app.svc_selected += 1;
            }
        }
        KeyCode::Home | KeyCode::Char('g') => app.svc_selected = 0,
        KeyCode::End | KeyCode::Char('G') => {
            app.svc_selected = app.services.len().saturating_sub(1);
        }
        KeyCode::Enter | KeyCode::Char('o') => app.open_in_browser(),
        KeyCode::Char('y') => app.copy_url(),
        KeyCode::Char('r') => {
            if let Some(c) = app.client.clone() {
                let tx2 = tx.clone();
                tokio::spawn(async move {
                    match crate::kube::services::list_services(&c, None).await {
                        Ok(svcs) => {
                            let _ =
                                tx2.send(AppEvent::ServicesLoaded { services: svcs }).await;
                        }
                        Err(e) => {
                            let _ = tx2
                                .send(AppEvent::Error {
                                    msg: format!("Refresh failed: {e}"),
                                })
                                .await;
                        }
                    }
                });
            }
        }
        KeyCode::Esc | KeyCode::Backspace => {
            for h in app.bg_handles.drain(..) {
                h.abort();
            }
            let ips = app.alias_ips.clone();
            tokio::spawn(async move {
                crate::network::tun::cleanup_aliases(&ips).await;
                let _ = crate::network::resolver::uninstall().await;
            });
            app.alias_ips.clear();
            app.service_entries.clear();
            app.services.clear();
            app.client = None;
            app.connected_context = None;
            app.tun_device_name = None;
            app.service_cidr = None;
            app.screen = Screen::Contexts;
            app.show_toast("Disconnected", false);
        }
        _ => {}
    }
}

#[cfg(unix)]
fn is_elevated() -> bool {
    nix::unistd::geteuid().is_root()
}

#[cfg(windows)]
fn is_elevated() -> bool {
    // Check if running as Administrator via a simple heuristic:
    // try to read a protected system path
    std::fs::metadata("C:\\Windows\\System32\\config\\SAM").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use tui::app::{App, ContextRow, Mode, Screen};
    use crate::kube::context::KubeContext;
    use crate::kube::services::{KubeService, ServicePort};

    fn make_ctx_app(n: usize) -> App {
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

    fn make_svc_app(n: usize) -> App {
        let mut app = App::new();
        app.screen = Screen::Services;
        app.services = (0..n)
            .map(|i| KubeService {
                name: format!("svc-{i}"),
                namespace: "default".into(),
                service_type: "ClusterIP".into(),
                cluster_ip: format!("10.96.0.{i}"),
                ports: vec![ServicePort {
                    port: 80,
                    target_port: "80".into(),
                    protocol: "TCP".into(),
                    name: None,
                }],
            })
            .collect();
        app
    }

    // ── handle_normal_key routing ───────────────────────────

    #[tokio::test]
    async fn test_normal_key_routes_to_ctx() {
        let mut app = make_ctx_app(3);
        let (tx, _rx) = mpsc::channel(10);
        handle_normal_key(&mut app, KeyCode::Char('q'), &tx);
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn test_normal_key_routes_to_svc() {
        let mut app = make_svc_app(3);
        let (tx, _rx) = mpsc::channel(10);
        handle_normal_key(&mut app, KeyCode::Char('q'), &tx);
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn test_normal_key_splash_noop() {
        let mut app = App::new(); // Screen::Splash
        let (tx, _rx) = mpsc::channel(10);
        handle_normal_key(&mut app, KeyCode::Char('q'), &tx);
        assert!(!app.should_quit); // splash ignores keys
    }

    // ── handle_ctx_key ──────────────────────────────────────

    #[tokio::test]
    async fn test_ctx_key_quit() {
        let mut app = make_ctx_app(3);
        let (tx, _rx) = mpsc::channel(10);
        handle_ctx_key(&mut app, KeyCode::Char('q'), &tx);
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn test_ctx_key_nav_down() {
        let mut app = make_ctx_app(3);
        let (tx, _rx) = mpsc::channel(10);
        handle_ctx_key(&mut app, KeyCode::Down, &tx);
        assert_eq!(app.ctx_selected, 1);
        handle_ctx_key(&mut app, KeyCode::Down, &tx);
        assert_eq!(app.ctx_selected, 2);
        handle_ctx_key(&mut app, KeyCode::Down, &tx);
        assert_eq!(app.ctx_selected, 2); // stays at end
    }

    #[tokio::test]
    async fn test_ctx_key_nav_up() {
        let mut app = make_ctx_app(3);
        app.ctx_selected = 2;
        let (tx, _rx) = mpsc::channel(10);
        handle_ctx_key(&mut app, KeyCode::Up, &tx);
        assert_eq!(app.ctx_selected, 1);
        handle_ctx_key(&mut app, KeyCode::Up, &tx);
        assert_eq!(app.ctx_selected, 0);
        handle_ctx_key(&mut app, KeyCode::Up, &tx);
        assert_eq!(app.ctx_selected, 0); // stays at start
    }

    #[tokio::test]
    async fn test_ctx_key_j_k_navigation() {
        let mut app = make_ctx_app(3);
        let (tx, _rx) = mpsc::channel(10);
        handle_ctx_key(&mut app, KeyCode::Char('j'), &tx);
        assert_eq!(app.ctx_selected, 1);
        handle_ctx_key(&mut app, KeyCode::Char('k'), &tx);
        assert_eq!(app.ctx_selected, 0);
    }

    #[tokio::test]
    async fn test_ctx_key_home() {
        let mut app = make_ctx_app(5);
        app.ctx_selected = 3;
        let (tx, _rx) = mpsc::channel(10);
        handle_ctx_key(&mut app, KeyCode::Home, &tx);
        assert_eq!(app.ctx_selected, 0);
    }

    #[tokio::test]
    async fn test_ctx_key_end() {
        let mut app = make_ctx_app(5);
        let (tx, _rx) = mpsc::channel(10);
        handle_ctx_key(&mut app, KeyCode::End, &tx);
        assert_eq!(app.ctx_selected, 4);
    }

    #[tokio::test]
    #[allow(non_snake_case)]
    async fn test_ctx_key_g_G_navigation() {
        let mut app = make_ctx_app(5);
        app.ctx_selected = 2;
        let (tx, _rx) = mpsc::channel(10);
        handle_ctx_key(&mut app, KeyCode::Char('g'), &tx);
        assert_eq!(app.ctx_selected, 0);
        handle_ctx_key(&mut app, KeyCode::Char('G'), &tx);
        assert_eq!(app.ctx_selected, 4);
    }

    #[tokio::test]
    async fn test_ctx_key_refresh() {
        let mut app = make_ctx_app(2);
        let (tx, _rx) = mpsc::channel(10);
        handle_ctx_key(&mut app, KeyCode::Char('r'), &tx);
        assert!(app.toast.is_some());
        assert_eq!(app.toast.as_ref().unwrap().message, "Refreshed");
    }

    #[tokio::test]
    async fn test_ctx_key_enter_triggers_connect() {
        let mut app = make_ctx_app(2);
        let (tx, _rx) = mpsc::channel(10);
        handle_ctx_key(&mut app, KeyCode::Enter, &tx);
        assert_eq!(app.mode, Mode::Connecting);
        assert_eq!(app.connected_context, Some("ctx-0".into()));
    }

    #[tokio::test]
    async fn test_ctx_key_unknown_noop() {
        let mut app = make_ctx_app(2);
        let (tx, _rx) = mpsc::channel(10);
        handle_ctx_key(&mut app, KeyCode::Char('x'), &tx);
        assert!(!app.should_quit);
        assert_eq!(app.ctx_selected, 0);
    }

    // ── handle_svc_key ──────────────────────────────────────

    #[tokio::test]
    async fn test_svc_key_quit() {
        let mut app = make_svc_app(3);
        let (tx, _rx) = mpsc::channel(10);
        handle_svc_key(&mut app, KeyCode::Char('q'), &tx);
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn test_svc_key_nav_down() {
        let mut app = make_svc_app(3);
        let (tx, _rx) = mpsc::channel(10);
        handle_svc_key(&mut app, KeyCode::Down, &tx);
        assert_eq!(app.svc_selected, 1);
        handle_svc_key(&mut app, KeyCode::Down, &tx);
        assert_eq!(app.svc_selected, 2);
        handle_svc_key(&mut app, KeyCode::Down, &tx);
        assert_eq!(app.svc_selected, 2); // stays at end
    }

    #[tokio::test]
    async fn test_svc_key_nav_up() {
        let mut app = make_svc_app(3);
        app.svc_selected = 2;
        let (tx, _rx) = mpsc::channel(10);
        handle_svc_key(&mut app, KeyCode::Up, &tx);
        assert_eq!(app.svc_selected, 1);
    }

    #[tokio::test]
    async fn test_svc_key_home_end() {
        let mut app = make_svc_app(5);
        app.svc_selected = 2;
        let (tx, _rx) = mpsc::channel(10);
        handle_svc_key(&mut app, KeyCode::Home, &tx);
        assert_eq!(app.svc_selected, 0);
        handle_svc_key(&mut app, KeyCode::End, &tx);
        assert_eq!(app.svc_selected, 4);
    }

    #[tokio::test]
    async fn test_svc_key_disconnect_esc() {
        let mut app = make_svc_app(2);
        app.connected_context = Some("dev".into());
        let (tx, _rx) = mpsc::channel(10);
        handle_svc_key(&mut app, KeyCode::Esc, &tx);
        assert_eq!(app.screen, Screen::Contexts);
        assert!(app.connected_context.is_none());
        assert!(app.services.is_empty());
        assert!(app.service_entries.is_empty());
    }

    #[tokio::test]
    async fn test_svc_key_disconnect_backspace() {
        let mut app = make_svc_app(2);
        app.connected_context = Some("dev".into());
        let (tx, _rx) = mpsc::channel(10);
        handle_svc_key(&mut app, KeyCode::Backspace, &tx);
        assert_eq!(app.screen, Screen::Contexts);
        assert!(app.toast.is_some());
        assert_eq!(app.toast.as_ref().unwrap().message, "Disconnected");
    }

    #[tokio::test]
    async fn test_svc_key_refresh_no_client_noop() {
        let mut app = make_svc_app(2);
        let (tx, _rx) = mpsc::channel(10);
        handle_svc_key(&mut app, KeyCode::Char('r'), &tx);
        assert_eq!(app.services.len(), 2);
    }

    // ── Navigation edge cases ───────────────────────────────

    #[tokio::test]
    async fn test_ctx_nav_empty_list() {
        let mut app = make_ctx_app(0);
        let (tx, _rx) = mpsc::channel(10);
        handle_ctx_key(&mut app, KeyCode::Down, &tx);
        assert_eq!(app.ctx_selected, 0);
        handle_ctx_key(&mut app, KeyCode::End, &tx);
        assert_eq!(app.ctx_selected, 0);
    }

    #[tokio::test]
    async fn test_svc_nav_empty_list() {
        let mut app = make_svc_app(0);
        let (tx, _rx) = mpsc::channel(10);
        handle_svc_key(&mut app, KeyCode::Down, &tx);
        assert_eq!(app.svc_selected, 0);
    }
}
