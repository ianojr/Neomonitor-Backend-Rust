use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Cell, Chart, Clear, Dataset, Gauge, GraphType, LegendPosition, Paragraph, Row,
    Scrollbar, ScrollbarOrientation, ScrollbarState, Sparkline, Table, Wrap,
};

use crate::app::{App, Panel, View};
use crate::calc;
use crate::diagnostics::{Confidence, Finding, Severity};
use crate::reading::Reading;
use crate::util::{bar_symbols, human_bytes, human_duration, human_rate, ratio};

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const TEXT: Color = Color::White;
const GOOD: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;
const GPU_C: Color = Color::Magenta;

const PROC_HEIGHT: u16 = 12;
/// Below this width three side-by-side columns stop being readable; switch to the compact
/// summary instead of squeezing panels.
const COLUMNS_MIN_W: u16 = 110;
/// The application refuses to squeeze below this; a clear message is drawn instead.
const MIN_W: u16 = 40;
const MIN_H: u16 = 8;

/// How the dashboard is presented for the current terminal geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DashMode {
    /// Three side-by-side columns (the original layout).
    Columns,
    /// One compact summary panel of essential metrics.
    Compact,
}

/// Result of the (pure) responsive-layout decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DashPlan {
    mode: DashMode,
    show_procs: bool,
    proc_height: u16,
    show_history: bool,
}

/// Decide the dashboard presentation for a terminal `width` and dashboard `body_height`
/// (the area below the header and above the footer).
///
/// Wide terminals with enough height keep the columns; everything narrower or shorter falls
/// back to the compact summary, which stays legible instead of clipping three columns.
fn dash_plan(width: u16, body_height: u16) -> DashPlan {
    let columns = width >= COLUMNS_MIN_W && body_height >= 20;
    let show_procs = body_height >= 22;
    let proc_height = if show_procs {
        PROC_HEIGHT.min(body_height.saturating_sub(10)).max(6)
    } else {
        0
    };
    let dash_h = body_height.saturating_sub(proc_height);
    DashPlan {
        mode: if columns {
            DashMode::Columns
        } else {
            DashMode::Compact
        },
        show_procs,
        proc_height,
        show_history: columns && dash_h >= 16,
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < MIN_W || area.height < MIN_H {
        draw_too_small(frame, area);
        return;
    }

    let body_h = area.height.saturating_sub(4); // header (3) + footer (1)
    let plan = dash_plan(area.width, body_h);
    let proc_h = if app.view == View::Dashboard && plan.show_procs {
        plan.proc_height
    } else {
        0
    };

    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(proc_h),
        Constraint::Length(1),
    ])
    .split(area);

    draw_header(frame, chunks[0], app);
    match app.view {
        View::Dashboard => {
            match plan.mode {
                DashMode::Columns => {
                    draw_dashboard_columns(frame, chunks[1], app, plan.show_history)
                }
                DashMode::Compact => draw_dashboard_compact(frame, chunks[1], app),
            }
            if proc_h > 0 {
                draw_processes(frame, chunks[2], app);
            }
        }
        View::Graphs => draw_graphs(frame, chunks[1], app),
        View::Investigation => draw_investigation(frame, chunks[1], app),
    }
    draw_footer(frame, chunks[3], app);

    if app.show_help {
        draw_help(frame, area, app);
    } else if app.pending_kill.is_some() {
        draw_confirm_kill(frame, area, app);
    } else if app.detail.is_some() {
        draw_proc_details(frame, area, app);
    }
}

fn draw_too_small(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let text = format!(
        "NeoMonitor needs at least {MIN_W}\u{00d7}{MIN_H} cells.\n\
         Current terminal is {}x{}.",
        area.width, area.height
    );
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(WARN)),
        area,
    );
}

fn base_panel<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::bordered()
        .title(title)
        .title_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(DIM))
}

fn focus_panel<'a>(focused: bool, title: impl Into<String>) -> Block<'a> {
    let text = if focused {
        format!(" {} \u{25c9} ", title.into())
    } else {
        format!(" {} ", title.into())
    };
    let border = if focused { ACCENT } else { DIM };
    Block::bordered()
        .title(Line::from(Span::styled(
            text,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )))
        .border_style(Style::default().fg(border))
}

fn usage_color(pct: f32) -> Color {
    if pct >= 85.0 {
        BAD
    } else if pct >= 60.0 {
        WARN
    } else {
        GOOD
    }
}

fn temp_color(t: f32) -> Color {
    if t >= 80.0 {
        BAD
    } else if t >= 65.0 {
        WARN
    } else {
        GOOD
    }
}

fn severity_color(s: Severity) -> Color {
    match s {
        Severity::Notice => ACCENT,
        Severity::Warning => WARN,
        Severity::Critical => BAD,
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

fn maybe_scrollbar(frame: &mut Frame, area: Rect, content_len: usize, offset: usize, page: usize) {
    if content_len <= page || content_len == 0 {
        return;
    }
    let mut state = ScrollbarState::new(content_len.saturating_sub(page)).position(offset);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .style(Style::default().fg(ACCENT))
        .begin_symbol(None)
        .end_symbol(None);
    frame.render_stateful_widget(bar, area, &mut state);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let cores = app.snap.cores.len();
    let phys = if app.physical_cores == 0 {
        cores
    } else {
        app.physical_cores
    };
    let clock = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let view = match app.view {
        View::Dashboard => "DASHBOARD",
        View::Graphs => "GRAPHS",
        View::Investigation => "INVESTIGATION",
    };

    let mut spans = vec![
        Span::styled(
            format!(" {} ", app.host),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("\u{2502} ", Style::default().fg(DIM)),
        Span::styled(app.os.clone(), Style::default().fg(TEXT)),
    ];
    // Drop the least important fields first on narrow terminals.
    if area.width >= 70 {
        spans.push(Span::styled(
            format!(" {}", app.arch),
            Style::default().fg(DIM),
        ));
    }
    spans.push(Span::styled("  \u{2502}  up ", Style::default().fg(DIM)));
    spans.push(Span::styled(
        human_duration(app.uptime_secs()),
        Style::default().fg(TEXT),
    ));
    spans.push(Span::styled(
        format!("  \u{2502}  {cores}t/{phys}c"),
        Style::default().fg(TEXT),
    ));
    if area.width >= 100 && !app.kernel.is_empty() {
        spans.push(Span::styled(
            format!("  \u{2502}  kernel {}", app.kernel),
            Style::default().fg(DIM),
        ));
    }
    spans.push(Span::styled(
        format!("   [{view}] "),
        Style::default().fg(GPU_C).add_modifier(Modifier::BOLD),
    ));
    if area.width >= 90 {
        spans.push(Span::styled(format!("{clock} "), Style::default().fg(WARN)));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(base_panel(Line::from(Span::styled(
            " NeoMonitor ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )))),
        area,
    );
}

fn draw_dashboard_columns(frame: &mut Frame, area: Rect, app: &mut App, show_history: bool) {
    let cols = Layout::horizontal([
        Constraint::Percentage(34),
        Constraint::Percentage(32),
        Constraint::Percentage(34),
    ])
    .split(area);

    draw_cpu_mem_column(frame, cols[0], app, show_history);
    draw_gpu_column(frame, cols[1], app, show_history);
    draw_network_column(frame, cols[2], app, show_history);
}

fn draw_cpu_mem_column(frame: &mut Frame, area: Rect, app: &mut App, show_history: bool) {
    let mut cons = vec![
        Constraint::Length(3), // CPU gauge
        Constraint::Min(6),    // per-core
    ];
    if show_history {
        cons.push(Constraint::Length(3)); // CPU history
    }
    cons.push(Constraint::Length(3)); // RAM
    cons.push(Constraint::Length(3)); // Swap
    if show_history {
        cons.push(Constraint::Length(3)); // RAM history
    }
    let rows = Layout::vertical(cons).split(area);

    let visible = rows[1].height.saturating_sub(2) as usize;
    app.set_page(Panel::Cores, visible);
    let start = app.offset(Panel::Cores);

    let cores = &app.snap.cores;
    let usage = app.snap.cpu_global;
    let avg_freq = if cores.is_empty() {
        0
    } else {
        cores.iter().map(|c| c.freq).sum::<u64>() / cores.len() as u64
    };
    let brand = &app.snap.cpu_brand;

    let gauge = Gauge::default()
        .block(base_panel(if brand.is_empty() {
            " CPU ".to_string()
        } else {
            format!(" CPU \u{2014} {brand} ")
        }))
        .gauge_style(Style::default().fg(usage_color(usage)).bg(Color::Black))
        .ratio((usage as f64 / 100.0).clamp(0.0, 1.0))
        .label(format!("{usage:.1}%   avg {avg_freq} MHz"));
    frame.render_widget(gauge, rows[0]);

    let core_count = cores.len();
    let core_lines: Vec<Line> = cores
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, core)| {
            let pct = core.usage;
            Line::from(vec![
                Span::styled(format!(" {i:>2} "), Style::default().fg(DIM)),
                Span::styled(bar_symbols(pct, 12), Style::default().fg(usage_color(pct))),
                Span::styled(format!(" {pct:>5.1}%"), Style::default().fg(TEXT)),
                Span::styled(format!("  {:>4} MHz", core.freq), Style::default().fg(DIM)),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(core_lines).block(focus_panel(app.is_focused(Panel::Cores), "Per-core")),
        rows[1],
    );
    maybe_scrollbar(frame, rows[1], core_count, start, visible);

    let mut idx = 2;
    if show_history {
        let cpu: Vec<u64> = app.cpu_history.iter().copied().collect();
        let s = calc::series_stat(&cpu);
        frame.render_widget(
            Sparkline::default()
                .block(base_panel(format!(
                    " CPU history % \u{2014} avg {} peak {} ",
                    s.avg, s.peak
                )))
                .data(&cpu)
                .max(100)
                .style(Style::default().fg(ACCENT)),
            rows[idx],
        );
        idx += 1;
    }

    let total = app.snap.mem.total;
    let used = app.snap.mem.used;
    let ram_pct = ratio(used, total) * 100.0;
    frame.render_widget(
        Gauge::default()
            .block(base_panel(" RAM "))
            .gauge_style(
                Style::default()
                    .fg(usage_color(ram_pct as f32))
                    .bg(Color::Black),
            )
            .ratio(ratio(used, total))
            .label(format!(
                "{} / {}  ({ram_pct:.1}%)",
                human_bytes(used),
                human_bytes(total)
            )),
        rows[idx],
    );
    idx += 1;

    let stotal = app.snap.mem.swap_total;
    let sused = app.snap.mem.swap_used;
    let swap_ratio = ratio(sused, stotal);
    frame.render_widget(
        Gauge::default()
            .block(base_panel(" Swap "))
            .gauge_style(
                Style::default()
                    .fg(if stotal == 0 {
                        DIM
                    } else {
                        usage_color((swap_ratio * 100.0) as f32)
                    })
                    .bg(Color::Black),
            )
            .ratio(swap_ratio)
            .label(if stotal == 0 {
                "no swap configured".to_string()
            } else {
                format!("{} / {}", human_bytes(sused), human_bytes(stotal))
            }),
        rows[idx],
    );
    idx += 1;

    if show_history {
        let mem: Vec<u64> = app.mem_history.iter().copied().collect();
        let s = calc::series_stat(&mem);
        frame.render_widget(
            Sparkline::default()
                .block(base_panel(format!(
                    " RAM history % \u{2014} avg {} peak {} ",
                    s.avg, s.peak
                )))
                .data(&mem)
                .max(100)
                .style(Style::default().fg(ACCENT)),
            rows[idx],
        );
    }
}

fn draw_gpu_column(frame: &mut Frame, area: Rect, app: &mut App, show_history: bool) {
    let mut cons = vec![
        Constraint::Length(3), // GPU gauge
        Constraint::Length(3), // GPU memory
    ];
    if show_history {
        cons.push(Constraint::Length(4)); // GPU history
    }
    cons.push(Constraint::Min(6)); // sensors
    cons.push(Constraint::Min(6)); // disks
    let rows = Layout::vertical(cons).split(area);

    let g = &app.gpu_stats;
    let caps = app.caps;
    let name = &g.name;
    let driver = &g.driver;
    let adapter_count = g.adapters.len();

    let (util_ratio, util_text, util_color) = match g.utilization {
        Reading::Valid(v) => (
            (v as f64 / 100.0).clamp(0.0, 1.0),
            format!("{v:.1}%"),
            usage_color(v),
        ),
        Reading::Stale(v) => (
            (v as f64 / 100.0).clamp(0.0, 1.0),
            format!("{v:.1}% (stale)"),
            WARN,
        ),
        Reading::Unavailable => (
            0.0,
            if caps.gpu_utilization.is_available() {
                "n/a".to_string()
            } else {
                "n/a (unsupported OS)".to_string()
            },
            DIM,
        ),
    };

    let mut title_parts: Vec<String> = Vec::new();
    if !name.is_empty() {
        let kind = g.primary_adapter().map(|a| a.kind.short()).unwrap_or("gpu");
        title_parts.push(format!("{} [{kind}]", truncate(name, 22)));
    }
    if adapter_count > 1 {
        title_parts.push(format!("+{}", adapter_count - 1));
    }
    let gpu_title = if title_parts.is_empty() {
        " GPU ".to_string()
    } else {
        format!(" GPU \u{2014} {} ", title_parts.join(" "))
    };
    let mut gpu_label = util_text;
    if !driver.is_empty() {
        gpu_label.push_str(&format!("   driver {driver}"));
    }
    if let Some(t) = g.temperature.value() {
        gpu_label.push_str(&format!("   {t:.0}\u{00b0}C"));
    }
    frame.render_widget(
        Gauge::default()
            .block(base_panel(gpu_title))
            .gauge_style(Style::default().fg(util_color).bg(Color::Black))
            .ratio(util_ratio)
            .label(gpu_label),
        rows[0],
    );

    if adapter_count > 1 {
        let lines: Vec<Line> = g
            .adapters
            .iter()
            .map(|a| {
                let util = match a.utilization {
                    Reading::Valid(v) => format!("{v:>5.1}%"),
                    Reading::Stale(v) => format!("{v:>5.1}%*"),
                    Reading::Unavailable => "  n/a".to_string(),
                };
                Line::from(vec![
                    Span::styled(format!(" {:<5}", a.kind.short()), Style::default().fg(DIM)),
                    Span::styled(
                        format!("{:<18}", truncate(&a.name, 18)),
                        Style::default().fg(TEXT),
                    ),
                    Span::styled(util, Style::default().fg(GPU_C)),
                    Span::styled(
                        format!("  {}", reading_bytes(&a.dedicated_used)),
                        Style::default().fg(DIM),
                    ),
                ])
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines).block(base_panel(" GPU adapters (dGPU/iGPU) ")),
            rows[1],
        );
    } else {
        let (mem_ratio, mem_color) = match (g.dedicated_used.value(), g.dedicated_total.value()) {
            (Some(used), Some(total)) if *total > 0 => (
                ratio(*used, *total),
                usage_color((ratio(*used, *total) * 100.0) as f32),
            ),
            _ => (0.0, DIM),
        };
        frame.render_widget(
            Gauge::default()
                .block(base_panel(" GPU memory "))
                .gauge_style(Style::default().fg(mem_color).bg(Color::Black))
                .ratio(mem_ratio)
                .label(format!(
                    "dedicated {}   shared {}   total {}",
                    reading_bytes(&g.dedicated_used),
                    reading_bytes(&g.shared_used),
                    reading_bytes(&g.dedicated_total),
                )),
            rows[1],
        );
    }

    let mut idx = 2;
    if show_history {
        let gpu_data = app.gpu_spark();
        let s = app.gpu_stat();
        let title = if app.gpu_has_gaps() {
            format!(
                " GPU history % \u{2014} avg {} peak {} (gaps = n/a) ",
                s.avg, s.peak
            )
        } else {
            format!(" GPU history % \u{2014} avg {} peak {} ", s.avg, s.peak)
        };
        frame.render_widget(
            Sparkline::default()
                .block(base_panel(title))
                .data(&gpu_data)
                .max(100)
                .style(Style::default().fg(GPU_C)),
            rows[idx],
        );
        idx += 1;
    }

    draw_sensors(frame, rows[idx], app);
    draw_disks(frame, rows[idx + 1], app);
}

/// Render a byte reading, distinguishing "unknown" from "zero".
fn reading_bytes(r: &Reading<u64>) -> String {
    match r {
        Reading::Valid(v) => human_bytes(*v),
        Reading::Stale(v) => format!("{} (stale)", human_bytes(*v)),
        Reading::Unavailable => "n/a".to_string(),
    }
}

fn draw_sensors(frame: &mut Frame, area: Rect, app: &mut App) {
    let visible = area.height.saturating_sub(2) as usize;
    app.set_page(Panel::Sensors, visible);

    let comps = &app.snap.comps;
    let start = if comps.is_empty() {
        0
    } else {
        app.offset(Panel::Sensors)
            .min(comps.len().saturating_sub(1))
    };

    let mut lines: Vec<Line> = Vec::new();
    if comps.is_empty() {
        lines.push(Line::from(Span::styled(
            " No sensors reported by the OS.",
            Style::default().fg(WARN),
        )));
        lines.push(Line::from(Span::styled(
            " Windows often hides these. Run elevated or add a",
            Style::default().fg(DIM),
        )));
        lines.push(Line::from(Span::styled(
            " provider (LibreHardwareMonitor) to expose them.",
            Style::default().fg(DIM),
        )));
    } else {
        for c in comps.iter().skip(start).take(visible) {
            let t = c.temp;
            // `max` is sysinfo's running peak, not a limit; scale the bar against the
            // critical threshold when the platform exposes one.
            let scale = c.critical.filter(|v| *v > 0.0).unwrap_or(100.0);
            let pct = t.map(|v| (v / scale * 100.0).clamp(0.0, 100.0));
            let color = t.map(temp_color).unwrap_or(DIM);
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {:<18}", truncate(&c.label, 18)),
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    bar_symbols(pct.unwrap_or(0.0), 8),
                    Style::default().fg(color),
                ),
                Span::styled(
                    format!(
                        " {:>6}",
                        t.map(|v| format!("{v:.1}\u{00b0}C"))
                            .unwrap_or_else(|| "n/a".into())
                    ),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    c.max
                        .map(|m| format!(" peak {m:.0}\u{00b0}C"))
                        .unwrap_or_default(),
                    Style::default().fg(DIM),
                ),
                Span::styled(
                    c.critical
                        .map(|m| format!(" crit {m:.0}\u{00b0}C"))
                        .unwrap_or_default(),
                    Style::default().fg(WARN),
                ),
            ]));
        }
    }
    frame.render_widget(
        Paragraph::new(lines).block(focus_panel(
            app.is_focused(Panel::Sensors),
            "Temperature / Sensors",
        )),
        area,
    );
    if !comps.is_empty() {
        maybe_scrollbar(frame, area, comps.len(), start, visible);
    }
}

fn draw_disks(frame: &mut Frame, area: Rect, app: &mut App) {
    let header = Row::new(vec![
        Cell::from("Disk"),
        Cell::from("Mount"),
        Cell::from("FS"),
        Cell::from("Usage"),
        Cell::from("Free"),
        Cell::from("Size"),
    ])
    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));

    let visible = area.height.saturating_sub(3) as usize;
    app.set_page(Panel::Disks, visible);
    let start = app.offset(Panel::Disks);

    let disks = &app.snap.disks;

    let disk_rows: Vec<Row> = disks
        .iter()
        .skip(start)
        .take(visible)
        .map(|d| {
            let total = d.total;
            let avail = d.avail;
            let used = total.saturating_sub(avail);
            let pct = (ratio(used, total) * 100.0) as f32;
            Row::new(vec![
                Cell::from(truncate(&d.name, 12)),
                Cell::from(truncate(&d.mount, 10)),
                Cell::from(d.fs.clone()),
                Cell::from(format!("{:>3.0}%", pct)).style(Style::default().fg(usage_color(pct))),
                Cell::from(human_bytes(avail)),
                Cell::from(human_bytes(total)),
            ])
        })
        .collect();

    let io = &app.snap.disk_io;
    let io_title = match (io.read_rate.value(), io.write_rate.value()) {
        (Some(r), Some(w)) => format!("Disks \u{2014} R {}  W {}", human_rate(*r), human_rate(*w)),
        _ if app.caps.disk_activity.is_available() => "Disks \u{2014} I/O n/a".to_string(),
        _ => "Disks".to_string(),
    };

    frame.render_widget(
        Table::new(
            disk_rows,
            [
                Constraint::Min(6),
                Constraint::Length(10),
                Constraint::Length(5),
                Constraint::Length(6),
                Constraint::Length(9),
                Constraint::Length(9),
            ],
        )
        .header(header)
        .column_spacing(1)
        .block(focus_panel(app.is_focused(Panel::Disks), io_title)),
        area,
    );
    maybe_scrollbar(frame, area, disks.len(), start, visible);
}

fn draw_network_column(frame: &mut Frame, area: Rect, app: &mut App, show_history: bool) {
    let mut cons = vec![Constraint::Min(8)]; // interface list
    if show_history {
        cons.push(Constraint::Length(3)); // RX history
        cons.push(Constraint::Length(3)); // TX history
    }
    cons.push(Constraint::Min(6)); // system panel
    let rows = Layout::vertical(cons).split(area);

    let visible = (rows[0].height.saturating_sub(2 + 1) as usize) / 2;
    app.set_page(Panel::Network, visible.max(1));
    let start = app.offset(Panel::Network);

    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(" \u{2193} ", Style::default().fg(GOOD)),
        Span::styled(
            format!("{:<11}", human_rate(app.snap.net_rx_rate)),
            Style::default().fg(TEXT),
        ),
        Span::styled(" \u{2191} ", Style::default().fg(WARN)),
        Span::styled(
            format!("{:<11}", human_rate(app.snap.net_tx_rate)),
            Style::default().fg(TEXT),
        ),
        Span::styled(
            format!(
                " total \u{2193}{} \u{2191}{}",
                human_bytes(app.snap.net_rx_total),
                human_bytes(app.snap.net_tx_total)
            ),
            Style::default().fg(DIM),
        ),
    ])];
    for iface in app.snap.ifaces.iter().skip(start).take(visible.max(1)) {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:<16}", truncate(&iface.name, 16)),
                Style::default().fg(ACCENT),
            ),
            Span::styled(truncate(&iface.ip, 20), Style::default().fg(TEXT)),
            Span::styled(format!("  {}", iface.mac), Style::default().fg(DIM)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("   \u{2193} ", Style::default().fg(GOOD)),
            Span::styled(
                format!("{:<11}", human_rate(iface.rx_rate)),
                Style::default().fg(GOOD),
            ),
            Span::styled("\u{2191} ", Style::default().fg(WARN)),
            Span::styled(
                format!("{:<11}", human_rate(iface.tx_rate)),
                Style::default().fg(WARN),
            ),
            Span::styled(
                format!(
                    "\u{2211}\u{2193}{} \u{2191}{}",
                    human_bytes(iface.rx_total),
                    human_bytes(iface.tx_total)
                ),
                Style::default().fg(DIM),
            ),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).block(focus_panel(app.is_focused(Panel::Network), "Network")),
        rows[0],
    );
    maybe_scrollbar(frame, rows[0], app.snap.ifaces.len(), start, visible.max(1));

    let mut idx = 1;
    if show_history {
        let rx: Vec<u64> = app.net_rx_history.iter().copied().collect();
        let rx_stat = calc::series_stat(&rx);
        let rx_max = rx_stat.peak.max(1);
        frame.render_widget(
            Sparkline::default()
                .block(base_panel(format!(
                    " \u{2193} RX history \u{2014} avg {} peak {} ",
                    human_rate(rx_stat.avg),
                    human_rate(rx_stat.peak)
                )))
                .data(&rx)
                .max(rx_max)
                .style(Style::default().fg(GOOD)),
            rows[idx],
        );
        idx += 1;

        let tx: Vec<u64> = app.net_tx_history.iter().copied().collect();
        let tx_stat = calc::series_stat(&tx);
        let tx_max = tx_stat.peak.max(1);
        frame.render_widget(
            Sparkline::default()
                .block(base_panel(format!(
                    " \u{2191} TX history \u{2014} avg {} peak {} ",
                    human_rate(tx_stat.avg),
                    human_rate(tx_stat.peak)
                )))
                .data(&tx)
                .max(tx_max)
                .style(Style::default().fg(WARN)),
            rows[idx],
        );
        idx += 1;
    }

    let load = app.snap.load;
    let sys_lines = vec![
        kv(" Processes", app.snap.total_procs.to_string()),
        kv(
            " Memory",
            format!(
                "{} / {}",
                human_bytes(app.snap.mem.used),
                human_bytes(app.snap.mem.total)
            ),
        ),
        kv(" Available", human_bytes(app.snap.mem.available)),
        kv(" Host", app.host.clone()),
        kv(
            " Load 1/5/15",
            format!("{:.2}  {:.2}  {:.2}", load.one, load.five, load.fifteen),
        ),
        kv(" OS", app.os.clone()),
        kv(" Distribution", app.distribution.clone()),
    ];
    frame.render_widget(
        Paragraph::new(sys_lines).block(base_panel(" System ")),
        rows[idx],
    );
}

fn kv(key: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<14}"), Style::default().fg(DIM)),
        Span::styled(value, Style::default().fg(TEXT)),
    ])
}

/// A one-line gauge used by the compact dashboard: ` LABEL [bar]  NN.N%  detail`.
fn compact_gauge(
    label: &str,
    pct: f32,
    color: Color,
    detail: String,
    bar_w: usize,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {label:<4}"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(bar_symbols(pct, bar_w), Style::default().fg(color)),
        Span::styled(
            format!(" {:>5.1}%  ", pct.clamp(0.0, 100.0)),
            Style::default().fg(TEXT),
        ),
        Span::styled(detail, Style::default().fg(DIM)),
    ])
}

/// A one-line "cannot measure this" note that explains why, instead of drawing a zero.
fn compact_note(label: &str, text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {label:<4}"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {text}"), Style::default().fg(WARN)),
    ])
}

/// Compact single-column dashboard for narrow or short terminals.
///
/// Rather than clipping three columns, it shows the essential metrics as legible one-line
/// rows, ordered by importance; anything that does not fit is simply not drawn.
fn draw_dashboard_compact(frame: &mut Frame, area: Rect, app: &App) {
    let inner = area.width.saturating_sub(2) as usize;
    let bar_w = inner.saturating_sub(32).clamp(4, 44);

    let snap = &app.snap;
    let cores = &snap.cores;
    let avg_freq = if cores.is_empty() {
        0
    } else {
        cores.iter().map(|c| c.freq).sum::<u64>() / cores.len() as u64
    };

    let mut lines: Vec<Line> = Vec::new();

    lines.push(compact_gauge(
        "CPU",
        snap.cpu_global,
        usage_color(snap.cpu_global),
        format!("avg {avg_freq} MHz, {} cores", cores.len()),
        bar_w,
    ));

    let ram_pct = (ratio(snap.mem.used, snap.mem.total) * 100.0) as f32;
    lines.push(compact_gauge(
        "MEM",
        ram_pct,
        usage_color(ram_pct),
        format!(
            "{} / {}",
            human_bytes(snap.mem.used),
            human_bytes(snap.mem.total)
        ),
        bar_w,
    ));

    match app.gpu_stats.utilization {
        Reading::Valid(v) => {
            let name = &app.gpu_stats.name;
            let mut detail = if name.is_empty() {
                "GPU".to_string()
            } else {
                truncate(name, 28)
            };
            if app.gpu_stats.adapters.len() > 1 {
                detail.push_str(&format!("  ({} adapters)", app.gpu_stats.adapters.len()));
            }
            lines.push(compact_gauge("GPU", v, usage_color(v), detail, bar_w));
        }
        Reading::Stale(v) => lines.push(compact_gauge(
            "GPU",
            v,
            WARN,
            "stale \u{2014} last value, not refreshed".to_string(),
            bar_w,
        )),
        Reading::Unavailable => {
            let note = if app.caps.gpu_utilization.is_available() {
                "n/a \u{2014} no GPU reading right now"
            } else {
                "n/a \u{2014} no GPU backend on this platform"
            };
            lines.push(compact_note("GPU", note));
        }
    }

    lines.push(Line::from(vec![
        Span::styled(
            " NET ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "\u{2193} {:<11} \u{2191} {:<11}",
                human_rate(snap.net_rx_rate),
                human_rate(snap.net_tx_rate)
            ),
            Style::default().fg(TEXT),
        ),
        Span::styled(
            format!(
                " total \u{2193}{} \u{2191}{}",
                human_bytes(snap.net_rx_total),
                human_bytes(snap.net_tx_total)
            ),
            Style::default().fg(DIM),
        ),
    ]));

    if let Some(d) = snap
        .disks
        .iter()
        .max_by_key(|d| d.total.saturating_sub(d.avail))
    {
        let used = d.total.saturating_sub(d.avail);
        let pct = (ratio(used, d.total) * 100.0) as f32;
        lines.push(compact_gauge(
            "DISK",
            pct,
            usage_color(pct),
            format!(
                "{} \u{2014} {} free of {}",
                truncate(&d.mount, 12),
                human_bytes(d.avail),
                human_bytes(d.total)
            ),
            bar_w,
        ));
    }

    if snap.mem.swap_total > 0 {
        let pct = (ratio(snap.mem.swap_used, snap.mem.swap_total) * 100.0) as f32;
        lines.push(compact_gauge(
            "SWAP",
            pct,
            usage_color(pct),
            format!(
                "{} / {}",
                human_bytes(snap.mem.swap_used),
                human_bytes(snap.mem.swap_total)
            ),
            bar_w,
        ));
    }

    if snap.comps.is_empty() {
        lines.push(compact_note(
            "TEMP",
            "no sensors reported by the OS (see ? help)",
        ));
    } else {
        let hottest = snap
            .comps
            .iter()
            .filter_map(|c| c.temp.map(|t| (t, c)))
            .max_by(|a, b| a.0.total_cmp(&b.0));
        match hottest {
            Some((t, c)) => lines.push(Line::from(vec![
                Span::styled(
                    " TEMP ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{t:.1}\u{00b0}C"),
                    Style::default()
                        .fg(temp_color(t))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        " hottest of {} ({})",
                        snap.comps.len(),
                        truncate(&c.label, 24)
                    ),
                    Style::default().fg(DIM),
                ),
            ])),
            None => lines.push(compact_note(
                "TEMP",
                "sensors present but no temperature readings",
            )),
        }
    }

    if !cores.is_empty() {
        let mut s = String::new();
        for c in cores {
            s.push_str(&format!("{:>3} ", c.usage.round() as i64));
        }
        lines.push(Line::from(vec![
            Span::styled(
                " CORE ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(s, Style::default().fg(TEXT)),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines).block(base_panel(" System summary ")),
        area,
    );
}

fn help_line<'a>(key: &str, desc: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {key:<16}"), Style::default().fg(ACCENT)),
        Span::styled(desc.to_string(), Style::default().fg(TEXT)),
    ])
}

/// Modal help overlay: shortcuts plus a short, honest capability report.
fn draw_help(frame: &mut Frame, area: Rect, app: &App) {
    let w = area.width.saturating_sub(4).clamp(40, 78);
    let h = area.height.saturating_sub(2).clamp(8, 30);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);
    frame.render_widget(Clear, rect);

    let gpu = if !app.caps.gpu_utilization.is_available() {
        "GPU: unsupported on this platform (no backend)".to_string()
    } else {
        match app.gpu_stats.utilization {
            Reading::Unavailable => "GPU: supported, no reading right now".to_string(),
            Reading::Stale(_) => "GPU: stale (last value retained)".to_string(),
            Reading::Valid(_) => {
                let n = app.gpu_stats.adapters.len();
                if n > 1 {
                    format!("GPU: available ({n} adapters)")
                } else {
                    "GPU: available".to_string()
                }
            }
        }
    };
    let sensors = if app.snap.comps.is_empty() {
        "CPU temperature: none reported by the OS (try elevated or a provider)".to_string()
    } else {
        "CPU temperature: available".to_string()
    };
    let capability = |support: crate::platform::Support| {
        support
            .detail()
            .map(|reason| format!("{} \u{2014} {reason}", support.label()))
            .unwrap_or_else(|| support.label().to_string())
    };
    let disk_activity = capability(app.caps.disk_activity);
    let gpu_temp = capability(app.caps.gpu_temperature);
    let power = capability(app.caps.power_consumption);

    let lines = vec![
        Line::from(Span::styled(
            " Keys",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        help_line("q / Esc", "quit"),
        help_line("?", "toggle this help"),
        help_line("g", "cycle dashboard / graphs / investigation"),
        help_line("s", "cycle process sort (CPU/MEM/NAME/PID/AGE)"),
        help_line("/", "search processes by name or PID"),
        help_line("f", "cycle filter (all / active / memory)"),
        help_line("v", "toggle process tree view"),
        help_line("Enter", "show process details"),
        help_line("Space", "expand / collapse a tree node"),
        help_line("x", "terminate selected process (asks first)"),
        help_line("t", "cycle sampling preset (fast/balanced/eco/minimal)"),
        help_line("i", "investigation view (findings + evidence)"),
        help_line("d", "(investigation) enable / disable the selected rule"),
        help_line(
            "[ / ]",
            "(investigation) lower / raise the selected rule threshold",
        ),
        help_line("Tab", "focus next panel"),
        help_line(
            "j / k / \u{2191}\u{2193}",
            "move selection on Processes, else scroll panel",
        ),
        help_line("PgUp / PgDn", "move / scroll by a page"),
        help_line("Home / End", "jump to top / bottom of list"),
        Line::from(Span::styled(
            "  Process keys act on the dashboard, where the process table is shown.",
            Style::default().fg(DIM),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            " Capabilities",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(format!("  {gpu}"), Style::default().fg(DIM))),
        Line::from(Span::styled(
            format!("  {sensors}"),
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            format!(
                "  GPU temperature: {gpu_temp}   Disk activity: {disk_activity}   Power: {power}"
            ),
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            "  Terminate: explicit + confirmed; force-only on Windows (taskkill /F).",
            Style::default().fg(DIM),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            " n/a = not measurable; (stale) = last value could not be refreshed.",
            Style::default().fg(DIM),
        )),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(base_panel(" Help \u{2014} ? or Esc to close ")),
        rect,
    );
}

fn draw_processes(frame: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.is_focused(Panel::Processes);
    let header = Row::new(vec![
        Cell::from("PID"),
        Cell::from("Process"),
        Cell::from("State"),
        Cell::from("CPU %"),
        Cell::from("Memory"),
    ])
    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));

    let visible = area.height.saturating_sub(3) as usize;
    app.set_page(Panel::Processes, visible);
    let start = app.offset(Panel::Processes);
    let sel = app.sel;

    let mut rows: Vec<Row> = Vec::new();
    {
        let view = app.view_rows();
        let procs = &app.snap.procs;
        for (slot, vr) in view.iter().enumerate().skip(start).take(visible) {
            let Some(p) = procs.get(vr.idx) else {
                continue;
            };
            let indent = "  ".repeat(vr.depth as usize);
            let name = format!(
                "{indent}{}",
                truncate(&p.name, 30usize.saturating_sub(indent.len()))
            );
            let selected = focused && slot == sel;
            let mut row = Row::new(vec![
                Cell::from(p.pid.to_string()),
                Cell::from(name),
                Cell::from(p.state.label()),
                Cell::from(format!("{:>6.1}", p.cpu))
                    .style(Style::default().fg(usage_color(p.cpu))),
                Cell::from(human_bytes(p.mem)),
            ]);
            if selected {
                row = row.style(
                    Style::default()
                        .bg(ACCENT)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD),
                );
            }
            rows.push(row);
        }
    }
    let view_len = app.view_rows().len();

    if rows.is_empty() {
        rows.push(
            Row::new(vec![Cell::from(if view_len == 0 {
                "no process matches the current query or filter"
            } else {
                ""
            })])
            .style(Style::default().fg(DIM)),
        );
    }

    let mut title = format!("Processes \u{2014} sort {}", app.sort_by.label());
    title.push_str(&format!(" \u{00b7} {}", app.filter.label()));
    if app.tree {
        title.push_str(" \u{00b7} tree");
    }
    if !app.query.is_empty() {
        title.push_str(&format!(" \u{00b7} \"{}\"", truncate(&app.query, 20)));
    }

    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(8),
                Constraint::Min(20),
                Constraint::Length(9),
                Constraint::Length(9),
                Constraint::Length(12),
            ],
        )
        .header(header)
        .column_spacing(2)
        .block(focus_panel(focused, title)),
        area,
    );
    maybe_scrollbar(frame, area, view_len, start, visible);
}

/// Modal process details overlay. Expensive fields are gathered on demand and may be
/// unavailable without elevation; that is shown honestly rather than as a fake value.
fn draw_proc_details(frame: &mut Frame, area: Rect, app: &App) {
    let Some(d) = &app.detail else {
        return;
    };
    let w = area.width.saturating_sub(4).clamp(40, 86);
    let h = area.height.saturating_sub(2).clamp(8, 24);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);
    frame.render_widget(Clear, rect);

    let dash = |s: &Option<String>| {
        s.clone()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "n/a".to_string())
    };
    let started = d
        .start_time
        .and_then(|s| chrono::DateTime::from_timestamp(s as i64, 0))
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".to_string());

    let lines = vec![
        kv("PID", d.pid.to_string()),
        kv("Name", d.name.clone()),
        kv("State", d.state.label().to_string()),
        kv(
            "Parent PID",
            d.ppid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "n/a".into()),
        ),
        kv(
            "User",
            d.user
                .clone()
                .unwrap_or_else(|| "n/a (needs privilege)".into()),
        ),
        kv("CPU", format!("{:.1}%", d.cpu)),
        kv(
            "Memory",
            format!(
                "{} resident  /  {} virtual",
                human_bytes(d.mem),
                human_bytes(d.virtual_mem)
            ),
        ),
        kv("Started", started),
        kv("Run time", human_duration(d.run_time)),
        kv("Executable", dash(&d.exe)),
        kv("Command", dash(&d.cmd)),
        kv("Working dir", dash(&d.cwd)),
        kv(
            "Total I/O",
            format!(
                "read {}  written {}",
                human_bytes(d.disk_read),
                human_bytes(d.disk_written)
            ),
        ),
        kv("Env vars", d.env_count.to_string()),
        Line::raw(""),
        Line::from(Span::styled(
            "  x terminate   Esc / Enter close",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(base_panel(" Process details ")),
        rect,
    );
}

/// Modal confirmation for a termination request. Termination is never automatic.
fn draw_confirm_kill(frame: &mut Frame, area: Rect, app: &App) {
    let Some(ident) = &app.pending_kill else {
        return;
    };
    let w = area.width.saturating_sub(4).clamp(40, 70);
    let h = area.height.saturating_sub(2).clamp(7, 10);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);
    frame.render_widget(Clear, rect);

    let lines = vec![
        Line::from(Span::styled(
            " Terminate this process?",
            Style::default().fg(BAD).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        kv("PID", ident.pid.to_string()),
        kv("Name", ident.name.clone()),
        Line::raw(""),
        Line::from(Span::styled(
            " On Windows this is a forceful kill (taskkill /F);",
            Style::default().fg(WARN),
        )),
        Line::from(Span::styled(
            " the process is not asked to exit cleanly.",
            Style::default().fg(WARN),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  y", Style::default().fg(BAD).add_modifier(Modifier::BOLD)),
            Span::styled(" confirm        ", Style::default().fg(TEXT)),
            Span::styled(
                "n / Esc",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" cancel", Style::default().fg(TEXT)),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(base_panel(" Confirm termination ")),
        rect,
    );
}

fn series_spans(
    name: &str,
    color: Color,
    s: calc::SeriesStat,
    fmt: impl Fn(u64) -> String,
) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!("{name} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} / {} / {}", fmt(s.current), fmt(s.avg), fmt(s.peak)),
            Style::default().fg(TEXT),
        ),
    ]
}

fn draw_graphs(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(5),
        Constraint::Length(1),
        Constraint::Fill(3),
    ])
    .split(area);

    let cpu: Vec<u64> = app.cpu_history.iter().copied().collect();
    let mem: Vec<u64> = app.mem_history.iter().copied().collect();
    let max_x = cpu.len().max(1) as f64;

    let cpu_pts: Vec<(f64, f64)> = cpu
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f64, *v as f64))
        .collect();
    let ram_pts: Vec<(f64, f64)> = mem
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f64, *v as f64))
        .collect();
    // Unavailable GPU windows are omitted, producing a visible gap rather than a drop to 0.
    let gpu_pts = app.gpu_points();

    let cpu_s = calc::series_stat(&cpu);
    let mem_s = calc::series_stat(&mem);
    let gpu_s = app.gpu_stat();
    let mut uspans = vec![Span::styled(
        " cur / avg / peak   ",
        Style::default().fg(DIM),
    )];
    uspans.extend(series_spans("CPU", GOOD, cpu_s, |v| format!("{v}%")));
    uspans.push(Span::raw("   "));
    uspans.extend(series_spans("RAM", ACCENT, mem_s, |v| format!("{v}%")));
    uspans.push(Span::raw("   "));
    uspans.extend(series_spans("GPU", GPU_C, gpu_s, |v| format!("{v}%")));
    if app.gpu_has_gaps() {
        uspans.push(Span::styled(
            "   (gaps = GPU n/a)",
            Style::default().fg(DIM),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(uspans)), chunks[0]);

    let chart = Chart::new(vec![
        Dataset::default()
            .name("CPU %")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(GOOD))
            .data(&cpu_pts),
        Dataset::default()
            .name("RAM %")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(ACCENT))
            .data(&ram_pts),
        Dataset::default()
            .name("GPU %")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(GPU_C))
            .data(&gpu_pts),
    ])
    .x_axis(
        Axis::default()
            .bounds([0.0, max_x])
            .labels(calc::span_axis_labels(app.history_span_secs())),
    )
    .y_axis(
        Axis::default()
            .bounds([0.0, 100.0])
            .labels(vec!["0", "25", "50", "75", "100"]),
    )
    .legend_position(Some(LegendPosition::TopRight))
    .block(base_panel(" Utilisation % \u{2014} CPU / RAM / GPU "));
    frame.render_widget(chart, chunks[1]);

    let rx: Vec<u64> = app.net_rx_history.iter().copied().collect();
    let tx: Vec<u64> = app.net_tx_history.iter().copied().collect();
    let rx_s = calc::series_stat(&rx);
    let tx_s = calc::series_stat(&tx);

    let mut nspans = vec![Span::styled(
        " cur / avg / peak   ",
        Style::default().fg(DIM),
    )];
    nspans.extend(series_spans("RX", GOOD, rx_s, human_rate));
    nspans.push(Span::raw("   "));
    nspans.extend(series_spans("TX", WARN, tx_s, human_rate));
    frame.render_widget(Paragraph::new(Line::from(nspans)), chunks[2]);

    let rx_pts: Vec<(f64, f64)> = rx
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f64, *v as f64))
        .collect();
    let tx_pts: Vec<(f64, f64)> = tx
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f64, *v as f64))
        .collect();
    let ymax = rx_s.peak.max(tx_s.peak).max(1) as f64;

    let net_chart = Chart::new(vec![
        Dataset::default()
            .name("RX")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(GOOD))
            .data(&rx_pts),
        Dataset::default()
            .name("TX")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(WARN))
            .data(&tx_pts),
    ])
    .x_axis(Axis::default().bounds([0.0, max_x]))
    .y_axis(Axis::default().bounds([0.0, ymax]).labels(vec![
        "0".to_string(),
        human_bytes((ymax / 2.0) as u64),
        human_bytes(ymax as u64),
    ]))
    .legend_position(Some(LegendPosition::TopRight))
    .block(base_panel(format!(
        " Network throughput \u{2014} peak {}/s ",
        human_bytes(ymax as u64)
    )));
    frame.render_widget(net_chart, chunks[3]);
}

/// The dedicated investigation view: a list of findings and the evidence behind the selected
/// one. It is read-only — nothing here changes system state.
fn draw_investigation(frame: &mut Frame, area: Rect, app: &App) {
    let now = app.snap.generated;
    let findings: Vec<&Finding> = app.inv.findings().iter().collect();

    if area.width >= 90 && area.height >= 12 {
        let cols = Layout::horizontal([Constraint::Percentage(44), Constraint::Percentage(56)])
            .split(area);
        draw_inv_list(frame, cols[0], app, &findings, now);
        draw_inv_detail(frame, cols[1], app, &findings, now);
    } else {
        let list_h = (area.height / 2).max(5);
        let rows = Layout::vertical([Constraint::Length(list_h), Constraint::Min(3)]).split(area);
        draw_inv_list(frame, rows[0], app, &findings, now);
        draw_inv_detail(frame, rows[1], app, &findings, now);
    }
}

fn draw_inv_list(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    findings: &[&Finding],
    now: std::time::Instant,
) {
    let header = Row::new(vec![
        Cell::from("  "),
        Cell::from("Rule"),
        Cell::from("Finding"),
        Cell::from("Age"),
    ])
    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));

    let visible = area.height.saturating_sub(3) as usize;
    let sel = app.inv_sel;
    let start = if visible == 0 || sel < visible {
        0
    } else {
        sel + 1 - visible
    };

    let mut rows: Vec<Row> = Vec::new();
    for (i, f) in findings.iter().enumerate().skip(start).take(visible) {
        let selected = i == sel;
        let sev = severity_color(f.severity);
        let mut row = Row::new(vec![
            Cell::from(Span::styled(f.severity.glyph(), Style::default().fg(sev))),
            Cell::from(Span::styled(
                f.rule.short().to_string(),
                Style::default().fg(sev),
            )),
            Cell::from(truncate(&f.title, 42)),
            Cell::from(human_duration(f.age(now).as_secs())),
        ]);
        if !f.active {
            row = row.style(Style::default().fg(DIM));
        }
        if selected {
            row = row.style(
                Style::default()
                    .bg(ACCENT)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            );
        }
        rows.push(row);
    }
    if rows.is_empty() {
        rows.push(
            Row::new(vec![Cell::from(
                "no findings \u{2014} no condition has crossed a configured threshold",
            )])
            .style(Style::default().fg(DIM)),
        );
    }

    let active = app.inv.active_count();
    let title = format!(
        "Findings \u{2014} {} active \u{00b7} {} logged ",
        active,
        findings.len()
    );
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(3),
                Constraint::Length(6),
                Constraint::Min(16),
                Constraint::Length(9),
            ],
        )
        .header(header)
        .column_spacing(1)
        .block(focus_panel(false, title)),
        area,
    );
    maybe_scrollbar(frame, area, findings.len(), start, visible);
}

fn draw_inv_detail(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    findings: &[&Finding],
    now: std::time::Instant,
) {
    let Some(f) = findings.get(app.inv_sel).copied() else {
        let rules = rule_legend(app);
        let mut lines = vec![
            Line::from(Span::styled(
                " No active condition.",
                Style::default().fg(GOOD).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                " Rules run on every sample; a finding appears only after a condition persists",
                Style::default().fg(DIM),
            )),
            Line::from(Span::styled(
                " past its window. This view is diagnostic only \u{2014} it never changes the system.",
                Style::default().fg(DIM),
            )),
            Line::raw(""),
        ];
        lines.extend(rules);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(base_panel(" Detail ")),
            area,
        );
        return;
    };

    let sev = severity_color(f.severity);
    let state = if f.active { "ACTIVE" } else { "recovered" };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {} ", f.severity.label().to_uppercase()),
                Style::default().fg(sev).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{state}  \u{00b7}  {} ", f.rule.label()),
                Style::default().fg(DIM),
            ),
        ]),
        Line::from(Span::styled(
            f.title.clone(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(
                " observed {} ago \u{00b7} active for {} \u{00b7} last seen {} ago",
                human_duration(f.age(now).as_secs()),
                human_duration(f.duration(now).as_secs()),
                human_duration(now.saturating_duration_since(f.last_seen).as_secs()),
            ),
            Style::default().fg(DIM),
        )),
        Line::raw(""),
        section("Detection rule"),
        Line::from(Span::styled(
            format!("  {}", f.detection),
            Style::default().fg(TEXT),
        )),
        Line::raw(""),
        section("Measured values"),
    ];
    for (k, v) in &f.values {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<18}"), Style::default().fg(DIM)),
            Span::styled(v.clone(), Style::default().fg(TEXT)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(section("Supporting evidence"));
    for e in &f.evidence {
        lines.push(Line::from(Span::styled(
            format!("  \u{2022} {e}"),
            Style::default().fg(TEXT),
        )));
    }
    lines.push(Line::raw(""));
    lines.push(section("Confidence"));
    let ccolor = match f.confidence {
        Confidence::High => GOOD,
        Confidence::Medium => WARN,
        Confidence::Low => BAD,
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {} ", f.confidence.label()),
            Style::default().fg(ccolor).add_modifier(Modifier::BOLD),
        ),
        Span::styled(f.confidence_note.clone(), Style::default().fg(DIM)),
    ]));
    lines.push(Line::raw(""));
    lines.push(section("Recommended next investigation"));
    lines.push(Line::from(Span::styled(
        format!("  {}", f.recommendation),
        Style::default().fg(TEXT),
    )));
    lines.push(Line::raw(""));
    lines.push(section("Limitations"));
    for l in &f.limitations {
        lines.push(Line::from(Span::styled(
            format!("  \u{2022} {l}"),
            Style::default().fg(DIM),
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        format!(
            " rule '{}': {} \u{00b7} threshold {}  (d toggle, [ ] adjust)",
            f.rule.short(),
            if app.inv.is_enabled(f.rule) {
                "enabled"
            } else {
                "disabled"
            },
            app.inv.threshold_text(f.rule),
        ),
        Style::default().fg(DIM),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(base_panel(" Evidence ")),
        area,
    );
}

fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {title}"),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))
}

fn rule_legend(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![section("Rules")];
    for rule in crate::diagnostics::RuleId::ALL {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<7}", rule.short()),
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!(
                    "{:<5} threshold {:<9} {}",
                    if app.inv.is_enabled(rule) {
                        "on"
                    } else {
                        "off"
                    },
                    app.inv.threshold_text(rule),
                    rule.label()
                ),
                Style::default().fg(DIM),
            ),
        ]));
    }
    lines
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    if app.searching {
        let line = Line::from(vec![
            Span::styled(
                " /",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" search: ", Style::default().fg(DIM)),
            Span::styled(app.query.clone(), Style::default().fg(TEXT)),
            Span::styled("\u{2588}", Style::default().fg(ACCENT)),
            Span::styled(
                "   Enter/Esc apply \u{00b7} Ctrl+U clear \u{00b7} Backspace edit",
                Style::default().fg(DIM),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    let sort = app.sort_by.label();
    let stats = app.stats();
    let age_ms = app.snap.generated.elapsed().as_secs_f64() * 1000.0;
    // Only surface health counters when something is actually wrong, to keep the bar quiet.
    let mut health = String::new();
    if stats.dropped > 0 {
        health.push_str(&format!("drop {} ", stats.dropped));
    }
    if stats.overruns > 0 {
        health.push_str(&format!("ovr {} ", stats.overruns));
    }
    if stats.gpu_failures > 0 {
        health.push_str(&format!("gpufail {} ", stats.gpu_failures));
    }
    let line = Line::from(vec![
        Span::styled(
            " q",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" quit  ", Style::default().fg(DIM)),
        Span::styled(
            "?",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" help  ", Style::default().fg(DIM)),
        Span::styled(
            "g",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" view  ", Style::default().fg(DIM)),
        Span::styled(
            "i",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        match app.inv.active_max_severity() {
            Some(sev) => {
                let unseen = app.inv.unseen();
                let mark = if unseen > 0 {
                    format!(" (+{unseen})")
                } else {
                    String::new()
                };
                Span::styled(
                    format!(" {:>2} findings{}  ", app.inv.active_count(), mark),
                    Style::default()
                        .fg(severity_color(sev))
                        .add_modifier(Modifier::BOLD),
                )
            }
            None => {
                let unseen = app.inv.unseen();
                if unseen > 0 {
                    Span::styled(format!(" ({unseen} new)  "), Style::default().fg(DIM))
                } else {
                    Span::styled(" investigation  ", Style::default().fg(DIM))
                }
            }
        },
        Span::styled(
            "s",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" sort({sort})  "), Style::default().fg(DIM)),
        Span::styled(
            "f",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" filter  ", Style::default().fg(DIM)),
        Span::styled(
            "v",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" tree  ", Style::default().fg(DIM)),
        Span::styled(
            "/",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" find  ", Style::default().fg(DIM)),
        Span::styled(
            "x",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" kill  ", Style::default().fg(DIM)),
        Span::styled(
            "Tab",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" focus: {}  ", app.focus.label()),
            Style::default().fg(DIM),
        ),
        Span::styled(
            "\u{2191}\u{2193}",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" scroll  ", Style::default().fg(DIM)),
        Span::styled(
            "t",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" speed: {}  ", app.preset.label()),
            Style::default().fg(DIM),
        ),
        Span::styled(
            format!("{:.0}fps {:.1}Hz  ", app.fps, app.sps),
            Style::default().fg(DIM),
        ),
        Span::styled(
            format!(
                "sample {:.1}ms age {:.0}ms ",
                app.snap.sample_cost.as_secs_f64() * 1000.0,
                age_ms
            ),
            Style::default().fg(DIM),
        ),
        Span::styled(health, Style::default().fg(WARN)),
        Span::styled(
            app.status_text()
                .map(|s| format!(" {s}"))
                .unwrap_or_default(),
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{CompStat, CoreStat, DiskStat, NetIface, ProcRow};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn populated_app() -> App {
        use crate::sampler::{Preset, SamplerConfig};
        use std::sync::Arc;
        let mut app = App::new(
            Arc::new(SamplerConfig::new(Preset::Balanced)),
            Preset::Balanced,
        );
        app.snap.cores = (0..8)
            .map(|i| CoreStat {
                usage: (i * 9) as f32,
                freq: 1800,
            })
            .collect();
        app.snap.cpu_global = 42.0;
        app.snap.cpu_brand = "Test CPU".to_string();
        app.snap.mem = crate::metrics::MemStat {
            total: 16 << 30,
            used: 10 << 30,
            available: 6 << 30,
            swap_total: 4 << 30,
            swap_used: 1 << 30,
        };
        app.snap.comps = vec![CompStat {
            label: "CPU Package".to_string(),
            temp: Some(55.0),
            max: Some(70.0),
            critical: Some(100.0),
        }];
        app.snap.disks = vec![DiskStat {
            name: "Disk C".to_string(),
            mount: "C:\\".to_string(),
            fs: "NTFS".to_string(),
            total: 512 << 30,
            avail: 200 << 30,
        }];
        app.snap.ifaces = vec![NetIface {
            name: "Wi-Fi".to_string(),
            rx_rate: 4096,
            tx_rate: 8192,
            rx_total: 1 << 20,
            tx_total: 2 << 20,
            ip: "10.0.0.5".to_string(),
            mac: "aa:bb:cc:dd:ee:ff".to_string(),
        }];
        app.snap.procs = (0..5)
            .map(|i| ProcRow {
                pid: i,
                name: format!("proc{i}"),
                cpu: i as f32,
                mem: 1 << 20,
                ppid: if i == 0 { None } else { Some(i - 1) },
                start_time: Some(1000 + i as u64),
                state: crate::metrics::ProcState::Run,
            })
            .collect();
        app.snap.total_procs = 200;
        app.gpu_stats.name = "Test GPU".to_string();
        app.gpu_stats.driver = "1.2.3".to_string();
        app.gpu_stats.utilization = Reading::Valid(30.0);
        app.gpu_stats.dedicated_used = Reading::Valid(1 << 30);
        app.gpu_stats.dedicated_total = Reading::Valid(4 << 30);
        app.gpu_stats.shared_used = Reading::Valid(256 << 20);
        for _ in 0..30 {
            app.cpu_history.push_back(20);
            app.mem_history.push_back(60);
            app.net_rx_history.push_back(1000);
            app.net_tx_history.push_back(2000);
            app.gpu_history.push_back(Some(30));
        }
        app
    }

    fn render(app: &mut App, w: u16, h: u16) {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
    }

    #[test]
    fn renders_at_many_terminal_sizes_without_panicking() {
        let mut app = populated_app();
        for (w, h) in [
            (20, 5),
            (40, 8),
            (60, 20),
            (80, 24),
            (100, 30),
            (160, 50),
            (200, 60),
        ] {
            render(&mut app, w, h);
            app.toggle_view();
            render(&mut app, w, h); // graphs view
            app.toggle_view();
            app.toggle_help();
            render(&mut app, w, h); // help overlay
            app.toggle_help();
        }
    }

    #[test]
    fn renders_investigation_view_with_findings() {
        use crate::diagnostics::{DiskFree, Sample};
        use std::time::{Duration, Instant};

        let mut app = populated_app();
        let t0 = Instant::now();
        for i in 0..=16u64 {
            let mut s = Sample::quiet(t0 + Duration::from_secs(i));
            s.cpu = Reading::Valid(95.0);
            s.disks = vec![DiskFree {
                name: "C:".to_string(),
                total: 100 << 30,
                avail: 1 << 30,
            }];
            app.inv.observe(&s);
        }
        assert!(app.inv_len() >= 1);
        app.open_investigation();

        for (w, h) in [(20, 5), (60, 20), (80, 24), (120, 40), (200, 60)] {
            render(&mut app, w, h);
        }
        // Selecting, toggling a rule and adjusting a threshold must not panic or desync.
        app.inv_move(1);
        render(&mut app, 120, 40);
        app.inv_toggle_rule();
        app.inv_adjust_threshold(true);
        app.inv_adjust_threshold(false);
        render(&mut app, 90, 30);
    }

    #[test]
    fn plan_columns_only_for_wide_and_tall() {
        let p = dash_plan(160, 40);
        assert_eq!(p.mode, DashMode::Columns);
        assert!(p.show_procs);
        assert!(p.show_history);

        // Wide but short must fall back to the compact summary.
        let p = dash_plan(160, 14);
        assert_eq!(p.mode, DashMode::Compact);
        assert!(!p.show_procs);

        // Narrow but tall must also use the compact summary.
        let p = dash_plan(80, 40);
        assert_eq!(p.mode, DashMode::Compact);

        // Exactly at the column threshold, with no room for the process table.
        let p = dash_plan(COLUMNS_MIN_W, 20);
        assert_eq!(p.mode, DashMode::Columns);
        assert!(!p.show_procs);
        assert!(p.show_history);

        // Column mode where processes eat the history rows.
        let p = dash_plan(160, 24);
        assert_eq!(p.mode, DashMode::Columns);
        assert!(p.show_procs);
        assert!(!p.show_history);
    }

    #[test]
    fn plan_never_over_allocates_height() {
        for h in 0..64u16 {
            let p = dash_plan(200, h);
            assert!(
                p.proc_height <= h,
                "proc_height {} > h {}",
                p.proc_height,
                h
            );
            let dash = h.saturating_sub(p.proc_height);
            assert!(dash <= h);
            if !p.show_procs {
                assert_eq!(p.proc_height, 0);
            }
        }
    }

    #[test]
    fn plan_handles_degenerate_geometry_without_panicking() {
        for (w, h) in [(0u16, 0u16), (1, 1), (MIN_W, MIN_H), (39, 7), (200, 0)] {
            let p = dash_plan(w, h);
            assert_eq!(p.proc_height, 0);
            assert!(!p.show_history);
        }
    }

    #[test]
    fn truncate_is_char_safe_and_bounded() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello", 8), "hello");
        assert_eq!(truncate("hello", 3), "he\u{2026}");
        assert_eq!(truncate("hi", 1), "\u{2026}");
        assert_eq!(truncate("", 3), "");
        // Multi-byte input is never split mid-character.
        let s = truncate("\u{6e29}\u{5ea6}\u{4f20}\u{611f}", 2);
        assert_eq!(s.chars().count(), 2);
    }

    #[test]
    fn renders_modal_overlays_without_panicking() {
        use crate::metrics::{ProcDetail, ProcIdentity};
        let mut app = populated_app();
        app.detail = Some(ProcDetail {
            pid: 42,
            name: "demo".to_string(),
            exe: Some("C:\\demo.exe".to_string()),
            cmd: Some("demo --flag".to_string()),
            user: Some("user".to_string()),
            cwd: Some("C:\\".to_string()),
            state: crate::metrics::ProcState::Run,
            ppid: Some(1),
            start_time: Some(1_700_000_000),
            run_time: 120,
            cpu: 3.5,
            mem: 1 << 20,
            virtual_mem: 2 << 20,
            disk_read: 4096,
            disk_written: 8192,
            env_count: 12,
        });
        for (w, h) in [(40, 8), (80, 24), (160, 50)] {
            render(&mut app, w, h);
        }
        app.detail = None;
        app.pending_kill = Some(ProcIdentity {
            pid: 42,
            name: "demo".to_string(),
            start_time: Some(1_700_000_000),
        });
        for (w, h) in [(40, 8), (80, 24), (160, 50)] {
            render(&mut app, w, h);
        }
    }

    #[test]
    fn renders_tree_and_search_states() {
        let mut app = populated_app();
        app.begin_query();
        for c in "proc".chars() {
            app.query_push(c);
        }
        render(&mut app, 120, 40);
        app.end_query();
        app.toggle_tree();
        app.toggle_expanded();
        render(&mut app, 120, 40);
    }
}
