mod app;
mod calc;
mod diagnostics;
mod diskio;
mod gpu;
mod metrics;
mod platform;
mod procs;
mod reading;
mod sampler;
mod ui;
mod util;

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;

use app::{App, Panel, View};
use metrics::Update;
use sampler::{Preset, Sampler, SamplerConfig};

/// How long the event loop blocks waiting for input. Also the maximum latency before newly
/// collected data is rendered. 50 ms keeps the terminal responsive (<=50 ms key latency)
/// while halving the wake-up rate of the previous 20 ms poll.
const POLL: Duration = Duration::from_millis(50);
const CLOCK_REDRAW: Duration = Duration::from_millis(1000);

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return help();
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("neomonitor {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--probe") {
        return probe();
    }
    if args.iter().any(|a| a == "--bench") {
        return bench();
    }

    let preset = parse_preset(&args);
    let cfg = Arc::new(SamplerConfig::new(preset));
    let mut sampler = sampler::spawn(cfg.clone());
    let mut app = App::new(cfg, preset);

    let mut terminal = ratatui::init();
    let _ = execute!(io::stdout(), EnableMouseCapture);

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        previous_hook(info);
    }));

    let result = run(&mut terminal, &mut app, &sampler);

    // Stop and join the workers before restoring the terminal.
    sampler.shutdown();

    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn parse_preset(args: &[String]) -> Preset {
    if let Some(i) = args.iter().position(|a| a == "--speed") {
        return match args.get(i + 1).map(String::as_str) {
            Some("fast") => Preset::Fast,
            Some("balanced") => Preset::Balanced,
            Some("eco") => Preset::Eco,
            Some("minimal") => Preset::Minimal,
            Some(other) => {
                eprintln!("neomonitor: unknown --speed '{other}', using 'balanced'");
                Preset::Balanced
            }
            None => {
                eprintln!(
                    "neomonitor: --speed needs a value (fast|balanced|eco|minimal), using 'balanced'"
                );
                Preset::Balanced
            }
        };
    }
    Preset::Balanced
}

fn help() -> io::Result<()> {
    println!(
        "neomonitor {} — realtime hardware monitor (TUI)\n\n\
         USAGE:\n    neomonitor [OPTIONS]\n\n\
         OPTIONS:\n\
         \x20   --speed <fast|balanced|eco|minimal>  sampling preset (default: balanced)\n\
         \x20   --probe                             print one measurement of every metric and exit\n\
         \x20   --bench                             time each collection source and exit\n\
         \x20   -h, --help                          print this help\n\
         \x20   -V, --version                       print the version\n\n\
         KEYS (interactive):\n\
         \x20   q / Esc   quit            g   cycle dashboard/graphs/investigation\n\
         \x20   i         investigation   d   (investigation) enable/disable rule\n\
         \x20   [ / ]     (investigation) lower/raise selected rule threshold\n\
         \x20   s         cycle proc sort  f  cycle proc filter\n\
         \x20   /         search procs     v  process tree\n\
         \x20   Enter     process details  x  terminate (asks first)\n\
         \x20   Space     expand/collapse tree node\n\
         \x20   t         cycle speed preset  ?  help overlay  Tab focus\n\
         \x20   arrows/j/k/PgUp/PgDn/Home/End  move selection on Processes, else scroll\n",
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    sampler: &Sampler,
) -> io::Result<()> {
    let mut last_draw = Instant::now() - CLOCK_REDRAW;
    let mut dirty = true;
    let mut quit = false;

    while !quit {
        let mut got = false;
        while let Ok(update) = sampler.rx.try_recv() {
            app.apply(update);
            got = true;
        }
        app.update_rates();

        if got || dirty || last_draw.elapsed() >= CLOCK_REDRAW {
            app.frame_tick();
            terminal.draw(|frame| ui::draw(frame, app))?;
            last_draw = Instant::now();
            dirty = false;
        }

        if event::poll(POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if app.show_help {
                        // The help overlay is modal: only dismiss or quit.
                        match key.code {
                            KeyCode::Char('?') | KeyCode::Esc => app.close_help(),
                            KeyCode::Char('q') => quit = true,
                            _ => {}
                        }
                    } else if app.pending_kill.is_some() {
                        // Confirmation is modal: nothing destructive happens without an explicit `y`.
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => app.confirm_kill(),
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                app.cancel_kill()
                            }
                            KeyCode::Char('q') => quit = true,
                            _ => {}
                        }
                    } else if app.detail.is_some() {
                        match key.code {
                            KeyCode::Esc | KeyCode::Enter => app.close_details(),
                            KeyCode::Char('x') => {
                                app.close_details();
                                if app.is_focused(Panel::Processes) {
                                    app.request_kill();
                                }
                            }
                            KeyCode::Char('q') => quit = true,
                            _ => {}
                        }
                    } else if app.searching {
                        // Text input mode captures printable keys instead of shortcuts.
                        match key.code {
                            KeyCode::Esc | KeyCode::Enter => app.end_query(),
                            KeyCode::Backspace => app.query_pop(),
                            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                app.query_clear()
                            }
                            KeyCode::Char(c) => app.query_push(c),
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('q') => quit = true,
                            KeyCode::Esc => quit = true,
                            KeyCode::Char('?') => app.toggle_help(),
                            KeyCode::Char('g') => app.toggle_view(),
                            KeyCode::Char('i') => app.open_investigation(),
                            KeyCode::Char('d') if app.view == View::Investigation => {
                                app.inv_toggle_rule();
                            }
                            KeyCode::Char('[') if app.view == View::Investigation => {
                                app.inv_adjust_threshold(false);
                            }
                            KeyCode::Char(']') if app.view == View::Investigation => {
                                app.inv_adjust_threshold(true);
                            }
                            // Process-scoped keys act only where the process table is drawn (the
                            // dashboard). On graphs/investigation the body is replaced, so a stale
                            // focus would otherwise let `x` stage a termination for an off-screen
                            // process.
                            KeyCode::Char('s') if app.process_keys_enabled() => app.toggle_sort(),
                            KeyCode::Char('f') if app.process_keys_enabled() => app.cycle_filter(),
                            KeyCode::Char('v') if app.process_keys_enabled() => app.toggle_tree(),
                            KeyCode::Char('/') if app.process_keys_enabled() => app.begin_query(),
                            KeyCode::Char('x')
                                if app.process_keys_enabled()
                                    && app.is_focused(Panel::Processes) =>
                            {
                                app.request_kill()
                            }
                            KeyCode::Enter
                                if app.process_keys_enabled()
                                    && app.is_focused(Panel::Processes) =>
                            {
                                app.open_details()
                            }
                            KeyCode::Char(' ')
                                if app.process_keys_enabled()
                                    && app.is_focused(Panel::Processes) =>
                            {
                                app.toggle_expanded()
                            }
                            KeyCode::Char('t') => app.cycle_preset(),
                            KeyCode::Tab => app.next_focus(),
                            KeyCode::Up | KeyCode::Char('k') => nav(app, -1),
                            KeyCode::Down | KeyCode::Char('j') => nav(app, 1),
                            KeyCode::PageUp => nav(app, -10),
                            KeyCode::PageDown => nav(app, 10),
                            KeyCode::Home => {
                                if app.view == View::Investigation {
                                    app.inv_sel = 0;
                                } else if app.is_focused(Panel::Processes) {
                                    app.selection_home()
                                } else {
                                    app.scroll_home()
                                }
                            }
                            KeyCode::End => {
                                if app.view == View::Investigation {
                                    app.inv_sel = app.inv_len().saturating_sub(1);
                                } else if app.is_focused(Panel::Processes) {
                                    app.selection_end()
                                } else {
                                    app.scroll_end()
                                }
                            }
                            _ => {}
                        }
                    }
                    dirty = true;
                }
                Event::Mouse(mouse) => {
                    let modal = app.show_help
                        || app.searching
                        || app.pending_kill.is_some()
                        || app.detail.is_some();
                    if !modal {
                        match mouse.kind {
                            MouseEventKind::ScrollDown => app.scroll(1),
                            MouseEventKind::ScrollUp => app.scroll(-1),
                            _ => {}
                        }
                        dirty = true;
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Arrow/`j`/`k`/page movement: in the investigation view it moves the finding cursor; on the
/// Processes panel it moves the selection cursor; on every other panel it scrolls as before.
fn nav(app: &mut App, delta: i32) {
    if app.view == View::Investigation {
        app.inv_move(delta);
    } else if app.is_focused(Panel::Processes) {
        app.move_selection(delta);
    } else {
        app.scroll(delta);
    }
}

fn probe() -> io::Result<()> {
    use sysinfo::{Components, Disks, Networks, System};

    let mut sys = System::new_all();
    sys.refresh_all();
    std::thread::sleep(Duration::from_millis(400));
    sys.refresh_cpu_usage();

    let freq_avg = if sys.cpus().is_empty() {
        0
    } else {
        sys.cpus().iter().map(|c| c.frequency()).sum::<u64>() / sys.cpus().len() as u64
    };

    println!("CPU global usage : {:.1}%", sys.global_cpu_usage());
    println!(
        "CPU per-core     : {:?}",
        sys.cpus()
            .iter()
            .map(|c| c.cpu_usage().round())
            .collect::<Vec<_>>()
    );
    println!("CPU freq avg     : {freq_avg} MHz");
    println!(
        "RAM              : {} / {} bytes",
        sys.used_memory(),
        sys.total_memory()
    );

    let disks = Disks::new_with_refreshed_list();
    println!("Disks            : {}", disks.list().len());
    let comps = Components::new_with_refreshed_list();
    println!(
        "Components       : {}{}",
        comps.list().len(),
        if comps.list().is_empty() {
            "  (no sensor provider exposed by the OS)"
        } else {
            ""
        }
    );

    let mut gpu = crate::gpu::GpuMonitor::new();

    let mut nets = Networks::new_with_refreshed_list();
    nets.refresh(true);
    std::thread::sleep(Duration::from_millis(1000));
    nets.refresh(true);
    let mut real = 0usize;
    let mut pseudo_count = 0usize;
    for (name, data) in nets.list() {
        let mac = data.mac_address();
        let ips: Vec<String> = data
            .ip_networks()
            .iter()
            .map(|n| n.addr.to_string())
            .collect();
        let pseudo = crate::calc::is_pseudo_interface(mac.is_unspecified(), !ips.is_empty());
        if pseudo {
            pseudo_count += 1;
        } else {
            real += 1;
        }
        println!(
            "NET {:<48} rx={:>8}/s tx={:>8}/s mac={} ip=[{}]{}",
            name,
            data.received(),
            data.transmitted(),
            mac,
            ips.join(","),
            if pseudo {
                "  [pseudo: excluded from totals]"
            } else {
                ""
            },
        );
    }
    println!("NET interfaces   : {real} real, {pseudo_count} pseudo (excluded)");

    match gpu.refresh() {
        Some(g) => {
            println!(
                "GPU adapters     : {} ({})",
                g.adapters.len(),
                if g.adapters.is_empty() {
                    "aggregate fallback"
                } else {
                    "DXGI + PDH"
                }
            );
            for (i, a) in g.adapters.iter().enumerate() {
                let marker = if g.primary == Some(i) { "*" } else { " " };
                println!(
                    "  {marker} {:<28} {:<10} util {}  ded {}  shared {}  vram {}  {}",
                    a.name,
                    a.kind.label(),
                    describe_f32(a.utilization),
                    describe_u64(a.dedicated_used),
                    describe_u64(a.shared_used),
                    describe_u64(a.dedicated_total),
                    a.driver,
                );
            }
            println!("GPU name         : {}", g.name);
            println!("GPU driver       : {}", g.driver);
            println!("GPU utilization  : {}", describe_f32(g.utilization));
            println!("GPU temperature  : {}", describe_celsius(g.temperature));
            println!(
                "GPU memory       : dedicated {}   shared {}   total {}",
                describe_u64(g.dedicated_used),
                describe_u64(g.shared_used),
                describe_u64(g.dedicated_total),
            );
        }
        None => println!("GPU              : unavailable (no backend)"),
    }

    let mut disk_io = crate::diskio::DiskIoMonitor::new();
    std::thread::sleep(Duration::from_millis(500));
    let io = disk_io.refresh();
    println!(
        "Disk activity    : read {}  write {}  (backend: {})",
        describe_bytes_rate(io.read_rate),
        describe_bytes_rate(io.write_rate),
        if disk_io.is_supported() {
            "PDH PhysicalDisk _Total"
        } else {
            "unsupported"
        }
    );

    println!("Capabilities (ceiling for this OS; runtime availability may be lower):");
    for (name, support) in crate::platform::Capabilities::detect().rows() {
        let status = if support.is_full() {
            "full"
        } else {
            support.label()
        };
        match support.detail() {
            Some(reason) => println!("  {name:<30} {status:<8} ({reason})"),
            None => println!("  {name:<30} {status}"),
        }
    }

    println!("Processes        : {}", sys.processes().len());
    Ok(())
}

fn describe_celsius(r: crate::reading::Reading<f32>) -> String {
    match r {
        crate::reading::Reading::Valid(v) => format!("{v:.1} C"),
        crate::reading::Reading::Stale(v) => format!("{v:.1} C (stale)"),
        crate::reading::Reading::Unavailable => "n/a".to_string(),
    }
}

fn describe_bytes_rate(r: crate::reading::Reading<u64>) -> String {
    match r {
        crate::reading::Reading::Valid(v) => crate::util::human_rate(v),
        crate::reading::Reading::Stale(v) => format!("{} (stale)", crate::util::human_rate(v)),
        crate::reading::Reading::Unavailable => "n/a".to_string(),
    }
}

fn describe_f32(r: crate::reading::Reading<f32>) -> String {
    match r {
        crate::reading::Reading::Valid(v) => format!("{v:.1}%"),
        crate::reading::Reading::Stale(v) => format!("{v:.1}% (stale)"),
        crate::reading::Reading::Unavailable => "n/a".to_string(),
    }
}

fn describe_u64(r: crate::reading::Reading<u64>) -> String {
    match r {
        crate::reading::Reading::Valid(v) => format!("{v} bytes"),
        crate::reading::Reading::Stale(v) => format!("{v} bytes (stale)"),
        crate::reading::Reading::Unavailable => "n/a".to_string(),
    }
}

fn bench() -> io::Result<()> {
    use sysinfo::{Components, Disks, Networks, ProcessesToUpdate, System};

    const BENCH_N: usize = 40;

    let mut sys = System::new_all();
    sys.refresh_all();
    let mut disks = Disks::new_with_refreshed_list();
    let mut nets = Networks::new_with_refreshed_list();
    let mut comps = Components::new_with_refreshed_list();
    let mut gpu = crate::gpu::GpuMonitor::new();

    println!(
        "per-source latency, {BENCH_N} samples each (release; ms) — p50/p95 are interpolated:"
    );
    let time = |name: &str, f: &mut dyn FnMut()| {
        let mut samples = Vec::with_capacity(BENCH_N);
        for _ in 0..BENCH_N {
            let t = Instant::now();
            f();
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let s = crate::util::summarize(&samples);
        println!(
            "  {name:<24} n={:<3} mean {:>7.3}  p50 {:>7.3}  p95 {:>7.3}  max {:>7.3}",
            s.n, s.mean, s.p50, s.p95, s.max
        );
    };

    time("refresh_cpu_usage", &mut || {
        sys.refresh_cpu_usage();
    });
    time("refresh_memory", &mut || {
        sys.refresh_memory();
    });
    time("refresh_cpu_frequency", &mut || {
        sys.refresh_cpu_frequency();
    });
    time("refresh_processes(lean)", &mut || {
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory(),
        );
    });
    time("networks.refresh", &mut || {
        nets.refresh(true);
    });
    time("disks.refresh", &mut || {
        disks.refresh(true);
    });
    time("components.refresh", &mut || {
        comps.refresh(true);
    });
    time("gpu refresh (engine+mem)", &mut || {
        gpu.refresh();
    });
    println!(
        "  sysinfo MINIMUM_CPU_UPDATE_INTERVAL = {:?}",
        sysinfo::MINIMUM_CPU_UPDATE_INTERVAL
    );

    // End-to-end: run the real pipeline and read its health counters.
    println!("\nlive sampler (balanced preset, ~3 s):");
    let cfg = Arc::new(SamplerConfig::new(Preset::Balanced));
    let mut sampler = sampler::spawn(cfg.clone());
    let run_for = Duration::from_secs(3);
    let start = Instant::now();
    let mut costs: Vec<f64> = Vec::new();
    let drain = |sampler: &Sampler, costs: &mut Vec<f64>| {
        while let Ok(update) = sampler.rx.try_recv() {
            if let Update::Sys(s) = update {
                costs.push(s.sample_cost.as_secs_f64() * 1000.0);
            }
        }
    };
    while start.elapsed() < run_for {
        drain(&sampler, &mut costs);
        std::thread::sleep(Duration::from_millis(50));
    }
    drain(&sampler, &mut costs);
    let elapsed = start.elapsed().as_secs_f64();
    sampler.shutdown();

    let stats = cfg.stats().snapshot();
    let cs = crate::util::summarize(&costs);
    println!(
        "  sys samples   : {} ({:.1}/s)",
        stats.sys_samples,
        stats.sys_samples as f64 / elapsed
    );
    println!(
        "  gpu samples   : {} ({:.1}/s)",
        stats.gpu_samples,
        stats.gpu_samples as f64 / elapsed
    );
    println!("  dropped       : {}", stats.dropped);
    println!("  overruns      : {}", stats.overruns);
    println!("  gpu failures  : {}", stats.gpu_failures);
    println!(
        "  sample cost   : n={} mean {:.2}  p95 {:.2}  max {:.2} ms",
        cs.n, cs.mean, cs.p95, cs.max
    );
    Ok(())
}
