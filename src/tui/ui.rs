use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};

use crate::tui::app::{App, Mode, Screen};

// ── Colors ───────────────────────────────────────────────

const FG: Color = Color::White;
const DIM: Color = Color::DarkGray;
const ACCENT: Color = Color::Cyan;
const GREEN: Color = Color::Green;
const YELLOW: Color = Color::Yellow;
const RED: Color = Color::Red;
const BORDER: Color = Color::DarkGray;
const SELECTED_BG: Color = Color::Rgb(30, 30, 46);
const HEADER_FG: Color = Color::Rgb(137, 180, 250);

// ── ASCII Art ────────────────────────────────────────────

const LOGO_BIG: &[&str] = &[
    "██████╗  ██████╗ ██████╗ ████████╗██╗  ██╗██╗   ██╗██████╗ ██████╗",
    "██╔══██╗██╔═══██╗██╔══██╗╚══██╔══╝██║ ██╔╝██║   ██║██╔══██╗██╔═══╝",
    "██████╔╝██║   ██║██████╔╝   ██║   █████╔╝ ██║   ██║██████╔╝█████╗ ",
    "██╔═══╝ ██║   ██║██╔══██╗   ██║   ██╔═██╗ ██║   ██║██╔══██╗██╔══╝ ",
    "██║     ╚██████╔╝██║  ██║   ██║   ██║  ██╗╚██████╔╝██████╔╝██████╗",
    "╚═╝      ╚═════╝ ╚═╝  ╚═╝   ╚═╝   ╚═╝  ╚═╝ ╚═════╝ ╚═════╝ ╚═════╝",
];

const LOGO_SMALL: &str = "█▀█ █▀█ █▀█ ▀█▀ █▄▀ █ █ █▀▄ █▀▀\n█▀▀ █ █ █▀▄  █  █▀▄ █ █ █▀▄ █▀▀\n▀   ▀▀▀ ▀ ▀  ▀  ▀ ▀ ▀▀▀ ▀▀  ▀▀▀";

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── Main render ──────────────────────────────────────────

pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();

    match app.screen {
        Screen::Splash => draw_splash(f, area, app),
        Screen::Contexts => {
            if app.mode == Mode::Connecting {
                draw_connecting_splash(f, area, app);
            } else {
                draw_main_layout(f, area, app, |f, content_area, app| {
                    draw_contexts(f, content_area, app);
                });
            }
        }
        Screen::Services => {
            draw_main_layout(f, area, app, |f, content_area, app| {
                draw_services(f, content_area, app);
            });
        }
    }
}

// ── Splash screen (shown while connecting) ───────────────

/// Startup splash — logo + progress bar while loading contexts.
fn draw_splash(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Percentage(25),
        Constraint::Length(9), // logo
        Constraint::Length(2), // spacer
        Constraint::Length(1), // progress bar
        Constraint::Length(1), // spacer
        Constraint::Length(1), // status
        Constraint::Percentage(30),
    ])
    .split(area);

    // Logo
    let mut lines: Vec<Line> = LOGO_BIG
        .iter()
        .map(|line| {
            Line::from(Span::styled(
                *line,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Kubernetes Local Development",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        format!("v{VERSION}"),
        Style::default().fg(DIM),
    )));

    let logo = Paragraph::new(lines).alignment(Alignment::Center);
    f.render_widget(logo, chunks[1]);

    // Progress bar
    let splash = &app.splash;
    let progress = splash.current_step as f64 / splash.total_steps.max(1) as f64;
    let bar_width = (chunks[3].width as usize).saturating_sub(20);
    let filled = (bar_width as f64 * progress) as usize;
    let empty = bar_width.saturating_sub(filled);

    let bar = Line::from(vec![
        Span::styled("  [", Style::default().fg(DIM)),
        Span::styled("█".repeat(filled), Style::default().fg(ACCENT)),
        Span::styled("░".repeat(empty), Style::default().fg(DIM)),
        Span::styled("]", Style::default().fg(DIM)),
        Span::styled(
            format!(" {}%", (progress * 100.0) as u8),
            Style::default().fg(FG),
        ),
    ]);
    let bar_para = Paragraph::new(bar).alignment(Alignment::Center);
    f.render_widget(bar_para, chunks[3]);

    // Status with spinner
    let spinner_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let spinner = spinner_frames[splash.spinner_frame % spinner_frames.len()];

    let status = Paragraph::new(Line::from(vec![
        Span::styled(format!("{spinner} "), Style::default().fg(YELLOW)),
        Span::styled(&splash.message, Style::default().fg(FG)),
    ]))
    .alignment(Alignment::Center);
    f.render_widget(status, chunks[5]);
}

/// Connecting splash — logo + progress bar + spinner status.
fn draw_connecting_splash(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Percentage(25),
        Constraint::Length(9), // logo
        Constraint::Length(2), // spacer
        Constraint::Length(1), // progress bar
        Constraint::Length(1), // spacer
        Constraint::Length(1), // status
        Constraint::Percentage(30),
    ])
    .split(area);

    // Logo
    let mut lines: Vec<Line> = LOGO_BIG
        .iter()
        .map(|line| {
            Line::from(Span::styled(
                *line,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Kubernetes Local Development",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        format!("v{VERSION}"),
        Style::default().fg(DIM),
    )));

    let logo = Paragraph::new(lines).alignment(Alignment::Center);
    f.render_widget(logo, chunks[1]);

    // Progress bar
    let splash = &app.splash;
    let progress = splash.current_step as f64 / splash.total_steps.max(1) as f64;
    let bar_width = (chunks[3].width as usize).saturating_sub(20);
    let filled = (bar_width as f64 * progress) as usize;
    let empty = bar_width.saturating_sub(filled);

    let bar = Line::from(vec![
        Span::styled("  [", Style::default().fg(DIM)),
        Span::styled("█".repeat(filled), Style::default().fg(ACCENT)),
        Span::styled("░".repeat(empty), Style::default().fg(DIM)),
        Span::styled("]", Style::default().fg(DIM)),
        Span::styled(
            format!(" {}%", (progress * 100.0) as u8),
            Style::default().fg(FG),
        ),
    ]);
    let bar_para = Paragraph::new(bar).alignment(Alignment::Center);
    f.render_widget(bar_para, chunks[3]);

    // Status with spinner
    let spinner_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let spinner = spinner_frames[splash.spinner_frame % spinner_frames.len()];

    let status = Paragraph::new(Line::from(vec![
        Span::styled(format!("{spinner} "), Style::default().fg(YELLOW)),
        Span::styled(&splash.message, Style::default().fg(FG)),
    ]))
    .alignment(Alignment::Center);
    f.render_widget(status, chunks[5]);
}

// ── Main layout (header + content + footer) ──────────────

fn draw_main_layout(
    f: &mut Frame,
    area: Rect,
    app: &App,
    draw_content: impl FnOnce(&mut Frame, Rect, &App),
) {
    let chunks = Layout::vertical([
        Constraint::Length(5), // header (logo + context info)
        Constraint::Min(0),    // content
        Constraint::Length(3), // footer
    ])
    .split(area);

    draw_header(f, chunks[0], app);
    draw_content(f, chunks[1], app);
    draw_status_bar(f, chunks[2], app);
}

// ── Header with small logo ───────────────────────────────

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let columns = Layout::horizontal([
        Constraint::Min(0),     // left: context info
        Constraint::Length(36), // right: small logo
    ])
    .split(area);

    // Left: context info
    let info = match &app.screen {
        Screen::Splash => {
            vec![]
        } // never rendered
        Screen::Contexts => {
            vec![
                Line::from(vec![
                    Span::styled(" ⎈ ", Style::default().fg(ACCENT).bold()),
                    Span::styled("Cluster Contexts", Style::default().fg(FG).bold()),
                ]),
                Line::from(Span::styled(
                    format!("   {} contexts found", app.contexts.len()),
                    Style::default().fg(DIM),
                )),
            ]
        }
        Screen::Services => {
            let ctx = app.connected_context.as_deref().unwrap_or("?");
            let proxied = app.service_entries.len();
            vec![
                Line::from(vec![
                    Span::styled(" ⎈ ", Style::default().fg(GREEN).bold()),
                    Span::styled(ctx, Style::default().fg(GREEN).bold()),
                    Span::styled(
                        format!("  {} services", app.services.len()),
                        Style::default().fg(DIM),
                    ),
                ]),
                Line::from(Span::styled(
                    format!("   {proxied} proxied  ·  use <svc>.<ns> in browser"),
                    Style::default().fg(DIM),
                )),
            ]
        }
    };

    let info_block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(BORDER));
    let info_para = Paragraph::new(info).block(info_block);
    f.render_widget(info_para, columns[0]);

    // Right: small logo
    let logo_lines: Vec<Line> = LOGO_SMALL
        .lines()
        .map(|l| {
            Line::from(Span::styled(
                l,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))
        })
        .chain(std::iter::once(Line::from(Span::styled(
            format!("v{VERSION}"),
            Style::default().fg(DIM),
        ))))
        .collect();

    let logo_block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(BORDER));
    let logo_para = Paragraph::new(logo_lines)
        .alignment(Alignment::Right)
        .block(logo_block);
    f.render_widget(logo_para, columns[1]);
}

// ── Context list ─────────────────────────────────────────

fn draw_contexts(f: &mut Frame, area: Rect, app: &App) {
    if app.contexts.is_empty() {
        let msg = Paragraph::new(vec![
            Line::raw(""),
            Line::styled("  No Kubernetes contexts found.", Style::default().fg(DIM)),
            Line::styled("  Check ~/.kube/config", Style::default().fg(DIM)),
        ]);
        f.render_widget(msg, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from("  Context"),
        Cell::from("Cluster"),
        Cell::from("Namespace"),
        Cell::from("User"),
    ])
    .style(Style::default().fg(HEADER_FG).add_modifier(Modifier::BOLD))
    .height(1)
    .bottom_margin(1);

    let rows: Vec<Row> = app
        .contexts
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let active_marker = if row.ctx.is_active { "●" } else { " " };

            let style = if i == app.ctx_selected {
                Style::default().bg(SELECTED_BG).fg(FG)
            } else {
                Style::default().fg(FG)
            };

            Row::new(vec![
                Cell::from(Line::from(vec![
                    Span::styled(
                        format!("{active_marker} "),
                        Style::default().fg(if row.ctx.is_active { GREEN } else { DIM }),
                    ),
                    Span::styled(&row.ctx.name, Style::default().fg(FG).bold()),
                ])),
                Cell::from(Span::styled(&row.ctx.cluster, Style::default().fg(DIM))),
                Cell::from(Span::styled(
                    &row.ctx.namespace,
                    Style::default().fg(ACCENT),
                )),
                Cell::from(Span::styled(&row.ctx.user, Style::default().fg(DIM))),
            ])
            .style(style)
            .height(1)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Min(28),
            Constraint::Min(24),
            Constraint::Min(14),
            Constraint::Min(14),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BORDER))
            .title(Span::styled(" Contexts ", Style::default().fg(FG).bold())),
    )
    .row_highlight_style(Style::default().bg(SELECTED_BG))
    .highlight_symbol("▸ ");

    let mut state = ratatui::widgets::TableState::default();
    state.select(Some(app.ctx_selected));
    f.render_stateful_widget(table, area, &mut state);
}

// ── Service list ─────────────────────────────────────────

fn draw_services(f: &mut Frame, area: Rect, app: &App) {
    if app.services.is_empty() {
        let msg = Paragraph::new(vec![
            Line::raw(""),
            Line::styled(
                "  No services found in this cluster.",
                Style::default().fg(DIM),
            ),
        ]);
        f.render_widget(msg, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from("  Service"),
        Cell::from("Namespace"),
        Cell::from("Type"),
        Cell::from("Ports"),
        Cell::from("DNS"),
        Cell::from("Status"),
    ])
    .style(Style::default().fg(HEADER_FG).add_modifier(Modifier::BOLD))
    .height(1)
    .bottom_margin(1);

    let rows: Vec<Row> = app
        .services
        .iter()
        .enumerate()
        .map(|(i, svc)| {
            let entry = app
                .service_entries
                .iter()
                .find(|e| e.name == svc.name && e.namespace == svc.namespace);

            let dns_name = format!("{}.{}", svc.name, svc.namespace);

            let (status_sym, status_color) = if entry.is_some() {
                ("● routed", GREEN)
            } else if svc.cluster_ip == "None" || svc.cluster_ip == "-" {
                ("○ headless", DIM)
            } else {
                ("○ pending", DIM)
            };

            let type_color = match svc.service_type.as_str() {
                "LoadBalancer" => GREEN,
                "NodePort" => YELLOW,
                _ => DIM,
            };

            let style = if i == app.svc_selected {
                Style::default().bg(SELECTED_BG).fg(FG)
            } else {
                Style::default().fg(FG)
            };

            Row::new(vec![
                Cell::from(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(&svc.name, Style::default().fg(FG).bold()),
                ])),
                Cell::from(Span::styled(&svc.namespace, Style::default().fg(ACCENT))),
                Cell::from(Span::styled(
                    &svc.service_type,
                    Style::default().fg(type_color),
                )),
                Cell::from(Span::styled(svc.ports_display(), Style::default().fg(DIM))),
                Cell::from(Span::styled(
                    dns_name,
                    Style::default().fg(if entry.is_some() { ACCENT } else { DIM }),
                )),
                Cell::from(Span::styled(status_sym, Style::default().fg(status_color))),
            ])
            .style(style)
            .height(1)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Min(14),
            Constraint::Min(14),
            Constraint::Min(14),
            Constraint::Min(22),
            Constraint::Min(12),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BORDER))
            .title(Span::styled(" Services ", Style::default().fg(FG).bold())),
    )
    .row_highlight_style(Style::default().bg(SELECTED_BG))
    .highlight_symbol("▸ ");

    let mut state = ratatui::widgets::TableState::default();
    state.select(Some(app.svc_selected));
    f.render_stateful_widget(table, area, &mut state);
}

// ── Status bar ───────────────────────────────────────────

fn draw_status_bar(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(BORDER));

    let help: Vec<Span> = match app.screen {
        Screen::Splash => {
            vec![]
        } // never rendered
        Screen::Contexts => {
            vec![
                key_span("↑↓", "navigate"),
                sep(),
                key_span("Enter", "connect"),
                sep(),
                key_span("r", "refresh"),
                sep(),
                key_span("q", "quit"),
            ]
        }
        Screen::Services => {
            vec![
                key_span("↑↓", "navigate"),
                sep(),
                key_span("Enter/o", "open browser"),
                sep(),
                key_span("y", "copy url"),
                sep(),
                key_span("r", "refresh"),
                sep(),
                key_span("Esc", "disconnect"),
                sep(),
                key_span("q", "quit"),
            ]
        }
    };

    let content = if let Some(toast) = &app.toast {
        let color = if toast.is_error { RED } else { GREEN };
        Line::from(vec![Span::styled(
            format!("  {}", toast.message),
            Style::default().fg(color),
        )])
    } else {
        Line::from(
            std::iter::once(Span::styled("  ", Style::default()))
                .chain(help)
                .collect::<Vec<_>>(),
        )
    };

    let paragraph = Paragraph::new(content).block(block);
    f.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{App, ContextRow, Mode, Screen, SplashState};
    use crate::kube::context::KubeContext;
    use crate::kube::services::{KubeService, ServicePort as KubeServicePort};
    use crate::network::tun::ServiceEntry;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn test_terminal() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(120, 40)).unwrap()
    }

    fn app_splash() -> App {
        App::new() // default is Screen::Splash
    }

    fn app_contexts(n: usize) -> App {
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

    fn app_services() -> App {
        let mut app = App::new();
        app.screen = Screen::Services;
        app.connected_context = Some("dev".into());
        app.services = vec![KubeService {
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
        }];
        app.service_entries = vec![ServiceEntry {
            name: "nginx".into(),
            namespace: "default".into(),
            port: 80,
            cluster_ip: std::net::Ipv4Addr::new(10, 96, 0, 10),
        }];
        app
    }

    // ── Logo constants ──────────────────────────────────────

    #[test]
    fn test_logo_big_has_six_lines() {
        assert_eq!(LOGO_BIG.len(), 6);
    }

    #[test]
    fn test_logo_small_has_three_lines() {
        assert_eq!(LOGO_SMALL.lines().count(), 3);
    }

    #[test]
    fn test_version_not_empty() {
        assert!(!VERSION.is_empty());
    }

    // ── Render: splash ──────────────────────────────────────

    #[test]
    fn test_render_splash_no_panic() {
        let mut terminal = test_terminal();
        let app = app_splash();
        terminal.draw(|f| render(f, &app)).unwrap();
    }

    // ── Render: contexts ────────────────────────────────────

    #[test]
    fn test_render_contexts_no_panic() {
        let mut terminal = test_terminal();
        let app = app_contexts(3);
        terminal.draw(|f| render(f, &app)).unwrap();
    }

    #[test]
    fn test_render_contexts_empty_no_panic() {
        let mut terminal = test_terminal();
        let app = app_contexts(0);
        terminal.draw(|f| render(f, &app)).unwrap();
    }

    // ── Render: connecting splash ───────────────────────────

    #[test]
    fn test_render_connecting_splash_no_panic() {
        let mut terminal = test_terminal();
        let mut app = app_contexts(1);
        app.mode = Mode::Connecting;
        app.splash = SplashState::new();
        app.splash.total_steps = 5;
        app.splash.advance("Connecting...");
        terminal.draw(|f| render(f, &app)).unwrap();
    }

    // ── Render: services ────────────────────────────────────

    #[test]
    fn test_render_services_no_panic() {
        let mut terminal = test_terminal();
        let app = app_services();
        terminal.draw(|f| render(f, &app)).unwrap();
    }

    #[test]
    fn test_render_services_empty_no_panic() {
        let mut terminal = test_terminal();
        let mut app = App::new();
        app.screen = Screen::Services;
        terminal.draw(|f| render(f, &app)).unwrap();
    }

    // ── Render: with toast ──────────────────────────────────

    #[test]
    fn test_render_with_toast_no_panic() {
        let mut terminal = test_terminal();
        let mut app = app_contexts(1);
        app.show_toast("Test toast", false);
        terminal.draw(|f| render(f, &app)).unwrap();
    }

    #[test]
    fn test_render_with_error_toast_no_panic() {
        let mut terminal = test_terminal();
        let mut app = app_services();
        app.show_toast("Error occurred", true);
        terminal.draw(|f| render(f, &app)).unwrap();
    }

    // ── Render: small terminal ──────────────────────────────

    #[test]
    fn test_render_small_terminal_no_panic() {
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        let app = app_services();
        terminal.draw(|f| render(f, &app)).unwrap();
    }

    // ── Render: headless & multi-type services ──────────────

    #[test]
    fn test_render_services_various_types_no_panic() {
        let mut terminal = test_terminal();
        let mut app = App::new();
        app.screen = Screen::Services;
        app.connected_context = Some("dev".into());
        app.services = vec![
            KubeService {
                name: "lb".into(), namespace: "default".into(),
                service_type: "LoadBalancer".into(), cluster_ip: "10.96.0.1".into(),
                ports: vec![KubeServicePort { port: 80, target_port: "80".into(), protocol: "TCP".into(), name: None }],
            },
            KubeService {
                name: "np".into(), namespace: "default".into(),
                service_type: "NodePort".into(), cluster_ip: "10.96.0.2".into(),
                ports: vec![KubeServicePort { port: 30080, target_port: "80".into(), protocol: "TCP".into(), name: None }],
            },
            KubeService {
                name: "headless".into(), namespace: "default".into(),
                service_type: "ClusterIP".into(), cluster_ip: "None".into(),
                ports: vec![],
            },
        ];
        terminal.draw(|f| render(f, &app)).unwrap();
    }
}

fn key_span<'a>(key: &'a str, label: &'a str) -> Span<'a> {
    Span::styled(format!(" {key} {label}"), Style::default().fg(DIM))
}

fn sep<'a>() -> Span<'a> {
    Span::styled("  ", Style::default())
}
