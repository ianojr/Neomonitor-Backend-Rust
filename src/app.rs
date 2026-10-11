use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sysinfo::System;

use crate::calc;
use crate::diagnostics::{Investigator, RuleId, Sample};
use crate::gpu::GpuStats;
use crate::metrics::{ProcDetail, ProcIdentity, ProcRow, SysSnapshot, Update};
use crate::platform::Capabilities;
use crate::procs::{self, Filter, ProcProbe, SortKey, TerminateOutcome, ViewOptions, ViewRow};
use crate::sampler::{Preset, SamplerConfig};
use crate::util::ratio;

pub const HISTORY: usize = 120;
/// How long a transient status message stays on screen.
const STATUS_TTL: Duration = Duration::from_secs(6);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Dashboard,
    Graphs,
    Investigation,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Cores,
    Processes,
    Disks,
    Sensors,
    Network,
}

impl Panel {
    pub const ALL: [Panel; 5] = [
        Panel::Cores,
        Panel::Processes,
        Panel::Disks,
        Panel::Sensors,
        Panel::Network,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        match self {
            Panel::Cores => "Cores",
            Panel::Processes => "Processes",
            Panel::Disks => "Disks",
            Panel::Sensors => "Sensors",
            Panel::Network => "Network",
        }
    }
}

pub struct App {
    pub snap: SysSnapshot,
    pub gpu_stats: GpuStats,

    pub cpu_history: VecDeque<u64>,
    pub mem_history: VecDeque<u64>,
    /// GPU utilisation history. `None` marks a window where the GPU reading was
    /// unavailable, so graphs can show a gap instead of a false zero.
    pub gpu_history: VecDeque<Option<u64>>,
    pub net_rx_history: VecDeque<u64>,
    pub net_tx_history: VecDeque<u64>,

    pub sort_by: SortKey,
    pub filter: Filter,
    pub tree: bool,
    pub query: String,
    pub searching: bool,
    expanded: HashSet<u32>,

    /// Visible process rows (index into `snap.procs` + tree depth), recomputed only when the
    /// collected data or the view options change — never during a redraw.
    view_rows: Vec<ViewRow>,
    pub sel: usize,

    pub detail: Option<ProcDetail>,
    pub pending_kill: Option<ProcIdentity>,
    status_msg: Option<(String, Instant)>,
    probe: Option<ProcProbe>,
    self_pid: u32,

    pub view: View,
    pub focus: Panel,
    pub show_help: bool,
    /// The platform's capability ceiling; lets the UI say "unsupported on this OS" instead
    /// of implying a metric is merely missing right now.
    pub caps: Capabilities,
    /// Deterministic condition detector fed from each completed snapshot.
    pub inv: Investigator,
    /// Selected row in the investigation view.
    pub inv_sel: usize,
    offsets: [usize; 5],
    pages: [usize; 5],

    pub preset: Preset,
    pub cfg: Arc<SamplerConfig>,

    pub host: String,
    pub os: String,
    pub kernel: String,
    pub arch: String,
    pub distribution: String,
    pub physical_cores: usize,

    /// System uptime (seconds) captured once, advanced locally by a monotonic clock so
    /// rendering never performs a system query.
    uptime_base: u64,
    uptime_at: Instant,

    frames: u32,
    samples: u32,
    last_rate: Instant,
    pub fps: f64,
    pub sps: f64,
}

impl App {
    pub fn new(cfg: Arc<SamplerConfig>, preset: Preset) -> Self {
        let physical_cores = System::physical_core_count().unwrap_or(0);

        App {
            snap: SysSnapshot::default(),
            gpu_stats: GpuStats::default(),
            cpu_history: VecDeque::with_capacity(HISTORY),
            mem_history: VecDeque::with_capacity(HISTORY),
            gpu_history: VecDeque::with_capacity(HISTORY),
            net_rx_history: VecDeque::with_capacity(HISTORY),
            net_tx_history: VecDeque::with_capacity(HISTORY),
            sort_by: SortKey::Cpu,
            filter: Filter::All,
            tree: false,
            query: String::new(),
            searching: false,
            expanded: HashSet::new(),
            view_rows: Vec::new(),
            sel: 0,
            detail: None,
            pending_kill: None,
            status_msg: None,
            probe: None,
            self_pid: std::process::id(),
            view: View::Dashboard,
            focus: Panel::Processes,
            show_help: false,
            caps: Capabilities::detect(),
            inv: Investigator::new(),
            inv_sel: 0,
            offsets: [0; 5],
            pages: [1; 5],
            preset,
            cfg,
            host: System::host_name().unwrap_or_else(|| "unknown".into()),
            os: System::long_os_version()
                .or_else(System::os_version)
                .unwrap_or_else(|| "unknown OS".into()),
            kernel: System::kernel_version().unwrap_or_default(),
            arch: System::cpu_arch(),
            distribution: System::distribution_id(),
            physical_cores,
            uptime_base: System::uptime(),
            uptime_at: Instant::now(),
            frames: 0,
            samples: 0,
            last_rate: Instant::now(),
            fps: 0.0,
            sps: 0.0,
        }
    }

    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Sys(snapshot) => self.apply_sys(*snapshot),
            Update::Gpu(stats) => self.gpu_stats = stats,
        }
    }

    fn apply_sys(&mut self, snapshot: SysSnapshot) {
        // All processes are kept; the visible subset is derived by `rebuild_proc_view`, so a
        // search or filter can see rows the previous top-N cap would have hidden.
        push(&mut self.cpu_history, snapshot.cpu_global.round() as u64);
        push(
            &mut self.mem_history,
            (ratio(snapshot.mem.used, snapshot.mem.total) * 100.0).round() as u64,
        );
        push(&mut self.net_rx_history, snapshot.net_rx_rate);
        push(&mut self.net_tx_history, snapshot.net_tx_rate);

        // GPU is sampled on its own (slower) cadence but must share the sys time axis.
        // An unavailable reading pushes `None` (a gap) rather than a fake zero; a stale
        // reading still carries its last value so the line stays continuous.
        let gpu_value = self
            .gpu_stats
            .utilization
            .value()
            .map(|v| calc::normalize_percent(*v as f64).round() as u64);
        push(&mut self.gpu_history, gpu_value);

        self.samples = self.samples.saturating_add(1);

        // Feed the diagnostic engine with a normalized view of this snapshot. The engine is
        // pure with respect to its inputs (no clock, no I/O) so its decisions are reproducible.
        let sample = Sample::from_snapshot(&snapshot, &self.gpu_stats, snapshot.generated);
        self.inv.observe(&sample);

        self.snap = snapshot;
        self.rebuild_proc_view();
        self.clamp_offsets();
        let findings = self.inv.findings().len();
        if self.inv_sel >= findings {
            self.inv_sel = findings.saturating_sub(1);
        }
    }

    fn clamp_offsets(&mut self) {
        for panel in Panel::ALL {
            let len = self.content_len(panel);
            // The page unknown until the next draw; use the stored page.
            let page = self.pages[panel.index()].max(1);
            let max = len.saturating_sub(page);
            if self.offsets[panel.index()] > max {
                self.offsets[panel.index()] = max;
            }
        }
    }

    // ----- process view -------------------------------------------------------

    fn rebuild_proc_view(&mut self) {
        let keep = self.selected_pid();
        let opts = ViewOptions {
            query: &self.query,
            filter: self.filter,
            sort: self.sort_by,
            tree: self.tree,
            expanded: &self.expanded,
        };
        let rows = procs::build_view(&self.snap.procs, &opts);
        self.view_rows = rows;

        if let Some(pid) = keep
            && let Some(pos) = self
                .view_rows
                .iter()
                .position(|vr| self.snap.procs[vr.idx].pid == pid)
        {
            self.sel = pos;
            return;
        }
        if self.sel >= self.view_rows.len() {
            self.sel = self.view_rows.len().saturating_sub(1);
        }
    }

    /// The currently visible process rows, in display order.
    pub fn view_rows(&self) -> &[ViewRow] {
        &self.view_rows
    }

    pub fn selected_row(&self) -> Option<&ProcRow> {
        let vr = self.view_rows.get(self.sel)?;
        self.snap.procs.get(vr.idx)
    }

    pub fn selected_pid(&self) -> Option<u32> {
        self.selected_row().map(|r| r.pid)
    }

    pub fn move_selection(&mut self, delta: i32) {
        if self.view_rows.is_empty() {
            self.sel = 0;
            return;
        }
        let max = self.view_rows.len() - 1;
        self.sel = (self.sel as i32 + delta).clamp(0, max as i32) as usize;
        self.ensure_selection_visible();
    }

    pub fn selection_home(&mut self) {
        self.sel = 0;
        self.ensure_selection_visible();
    }

    pub fn selection_end(&mut self) {
        self.sel = self.view_rows.len().saturating_sub(1);
        self.ensure_selection_visible();
    }

    fn ensure_selection_visible(&mut self) {
        let page = self.pages[Panel::Processes.index()].max(1);
        let off = &mut self.offsets[Panel::Processes.index()];
        if self.sel < *off {
            *off = self.sel;
        } else if self.sel >= *off + page {
            *off = self.sel + 1 - page;
        }
    }

    pub fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
        self.sel = 0;
        self.offsets[Panel::Processes.index()] = 0;
        self.rebuild_proc_view();
    }

    pub fn toggle_tree(&mut self) {
        self.tree = !self.tree;
        self.rebuild_proc_view();
    }

    pub fn toggle_expanded(&mut self) {
        if let Some(pid) = self.selected_pid() {
            if !self.expanded.remove(&pid) {
                self.expanded.insert(pid);
            }
            self.rebuild_proc_view();
        }
    }

    pub fn begin_query(&mut self) {
        self.searching = true;
    }

    pub fn end_query(&mut self) {
        self.searching = false;
    }

    pub fn query_push(&mut self, c: char) {
        self.query.push(c);
        self.refresh_query();
    }

    pub fn query_pop(&mut self) {
        self.query.pop();
        self.refresh_query();
    }

    pub fn query_clear(&mut self) {
        self.query.clear();
        self.refresh_query();
    }

    fn refresh_query(&mut self) {
        self.sel = 0;
        self.offsets[Panel::Processes.index()] = 0;
        self.rebuild_proc_view();
    }

    fn probe_mut(&mut self) -> &mut ProcProbe {
        self.probe.get_or_insert_with(ProcProbe::new)
    }

    pub fn open_details(&mut self) {
        let Some(pid) = self.selected_pid() else {
            return;
        };
        let logical = self.snap.cores.len().max(1);
        match self.probe_mut().detail(pid, logical) {
            Some(d) => self.detail = Some(d),
            None => self.set_status("details unavailable: process has exited"),
        }
    }

    pub fn close_details(&mut self) {
        self.detail = None;
    }

    /// Stage a termination request for explicit confirmation. Never kills on its own, and
    /// refuses to target this application's own process.
    pub fn request_kill(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.pid == self.self_pid {
            self.set_status("refusing to terminate NeoMonitor itself");
            return;
        }
        self.pending_kill = Some(ProcIdentity::of(row));
    }

    pub fn cancel_kill(&mut self) {
        self.pending_kill = None;
    }

    /// Execute the staged termination after revalidating the process identity.
    pub fn confirm_kill(&mut self) {
        let Some(ident) = self.pending_kill.take() else {
            return;
        };
        let outcome = self.probe_mut().terminate(&ident);
        if outcome == TerminateOutcome::Sent
            && self.detail.as_ref().map(|d| d.pid) == Some(ident.pid)
        {
            self.detail = None;
        }
        let msg = format!("PID {} ({}): {}", ident.pid, ident.name, outcome.message());
        self.set_status(msg);
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_msg = Some((msg.into(), Instant::now()));
    }

    pub fn status_text(&self) -> Option<&str> {
        self.status_msg
            .as_ref()
            .filter(|(_, at)| at.elapsed() < STATUS_TTL)
            .map(|(m, _)| m.as_str())
    }

    // ----- general ------------------------------------------------------------

    /// Wall-clock span covered by the history buffers, in seconds.
    pub fn history_span_secs(&self) -> u64 {
        let per_sample_ms = self.preset.fast_ms().max(1);
        (HISTORY as u64 * per_sample_ms).div_ceil(1000).max(1)
    }

    /// Health counters for the collection pipeline.
    pub fn stats(&self) -> crate::sampler::StatsSnapshot {
        self.cfg.stats().snapshot()
    }

    /// System uptime in seconds, advanced locally without a system query.
    pub fn uptime_secs(&self) -> u64 {
        self.uptime_base + self.uptime_at.elapsed().as_secs()
    }

    pub fn is_focused(&self, panel: Panel) -> bool {
        self.view == View::Dashboard && self.focus == panel
    }

    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    pub fn close_help(&mut self) {
        self.show_help = false;
    }

    /// GPU history as `(x, y)` points, skipping unavailable windows so the chart can show a
    /// gap instead of connecting a drop to zero.
    pub fn gpu_points(&self) -> Vec<(f64, f64)> {
        self.gpu_history
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.map(|val| (i as f64, val as f64)))
            .collect()
    }

    /// GPU history as a dense series for a sparkline (which cannot draw gaps): unavailable
    /// windows hold the last known value, starting at zero before any reading exists.
    pub fn gpu_spark(&self) -> Vec<u64> {
        let mut last = 0u64;
        self.gpu_history
            .iter()
            .map(|v| {
                if let Some(x) = v {
                    last = *x;
                }
                last
            })
            .collect()
    }

    /// Current/average/peak over only the windows where the GPU was available.
    pub fn gpu_stat(&self) -> calc::SeriesStat {
        let vals: Vec<u64> = self.gpu_history.iter().flatten().copied().collect();
        calc::series_stat(&vals)
    }

    /// True when at least one GPU window is unavailable (used to explain gaps in the UI).
    pub fn gpu_has_gaps(&self) -> bool {
        self.gpu_history.iter().any(Option::is_none)
    }

    pub fn toggle_sort(&mut self) {
        self.sort_by = self.sort_by.next();
        self.rebuild_proc_view();
    }

    pub fn toggle_view(&mut self) {
        self.view = match self.view {
            View::Dashboard => View::Graphs,
            View::Graphs => View::Investigation,
            View::Investigation => View::Dashboard,
        };
    }

    /// Whether process-scoped keys (sort / filter / tree / search / details / terminate) are
    /// active. The process table is drawn only on the dashboard; the graphs and investigation
    /// views replace the body, so a stale focus there would let these keys act on an
    /// off-screen process (notably `x`, which stages a termination).
    pub fn process_keys_enabled(&self) -> bool {
        self.view == View::Dashboard
    }

    /// Jump straight to the investigation view (and clear its "unseen" badge).
    pub fn open_investigation(&mut self) {
        self.view = View::Investigation;
        self.inv.mark_seen();
    }

    // ----- investigation view -------------------------------------------------

    pub fn inv_len(&self) -> usize {
        self.inv.findings().len()
    }

    pub fn inv_selected(&self) -> Option<&crate::diagnostics::Finding> {
        self.inv.findings().get(self.inv_sel)
    }

    pub fn inv_move(&mut self, delta: i32) {
        let len = self.inv_len();
        if len == 0 {
            self.inv_sel = 0;
            return;
        }
        let max = len - 1;
        self.inv_sel = (self.inv_sel as i32 + delta).clamp(0, max as i32) as usize;
        // Opening the view dismisses the unseen badge for the current set.
        self.inv.mark_seen();
    }

    /// Toggle the selected finding's rule on/off. Returns the rule if one was toggled.
    pub fn inv_toggle_rule(&mut self) -> Option<RuleId> {
        let rule = self.inv_selected()?.rule;
        let on = self.inv.toggle(rule);
        self.set_status(format!(
            "rule '{}' {}",
            rule.short(),
            if on { "enabled" } else { "disabled" }
        ));
        Some(rule)
    }

    /// Adjust the selected finding's rule primary threshold. Returns the new value text.
    pub fn inv_adjust_threshold(&mut self, up: bool) -> Option<String> {
        let rule = self.inv_selected()?.rule;
        let v = self.inv.adjust_threshold(rule, up);
        self.set_status(format!("rule '{}' threshold: {v}", rule.short()));
        Some(v)
    }

    pub fn cycle_preset(&mut self) {
        self.preset = self.preset.next();
        self.cfg.set(self.preset);
    }

    pub fn frame_tick(&mut self) {
        self.frames = self.frames.saturating_add(1);
    }

    pub fn update_rates(&mut self) {
        let elapsed = self.last_rate.elapsed();
        if elapsed >= std::time::Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64();
            self.fps = self.frames as f64 / secs;
            self.sps = self.samples as f64 / secs;
            self.frames = 0;
            self.samples = 0;
            self.last_rate = Instant::now();
        }
    }

    pub fn next_focus(&mut self) {
        let idx = (self.focus.index() + 1) % Panel::ALL.len();
        self.focus = Panel::ALL[idx];
    }

    fn static_len_of(&self, panel: Panel) -> usize {
        match panel {
            Panel::Cores => self.snap.cores.len(),
            Panel::Processes => self.view_rows.len(),
            Panel::Disks => self.snap.disks.len(),
            Panel::Sensors => self.snap.comps.len().max(1),
            Panel::Network => self.snap.ifaces.len(),
        }
    }

    pub fn content_len(&self, panel: Panel) -> usize {
        self.static_len_of(panel)
    }

    pub fn set_page(&mut self, panel: Panel, page: usize) {
        self.pages[panel.index()] = page.max(1);
    }

    pub fn offset(&self, panel: Panel) -> usize {
        self.offsets[panel.index()]
    }

    pub fn scroll(&mut self, delta: i32) {
        let panel = self.focus;
        let len = self.content_len(panel);
        let page = self.pages[panel.index()];
        let max = len.saturating_sub(page);
        let current = self.offsets[panel.index()] as i32;
        let next = (current + delta).clamp(0, max as i32) as usize;
        self.offsets[panel.index()] = next;
    }

    pub fn scroll_home(&mut self) {
        self.offsets[self.focus.index()] = 0;
    }

    pub fn scroll_end(&mut self) {
        let panel = self.focus;
        let len = self.content_len(panel);
        let page = self.pages[panel.index()];
        self.offsets[panel.index()] = len.saturating_sub(page);
    }
}

fn push<T>(buf: &mut VecDeque<T>, value: T) {
    if buf.len() >= HISTORY {
        buf.pop_front();
    }
    buf.push_back(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::ProcRow;

    fn app() -> App {
        App::new(
            Arc::new(SamplerConfig::new(Preset::Balanced)),
            Preset::Balanced,
        )
    }

    fn proc(pid: u32, name: &str, cpu: f32, ppid: Option<u32>) -> ProcRow {
        ProcRow {
            pid,
            name: name.to_string(),
            cpu,
            mem: 1 << 20,
            ppid,
            start_time: Some(pid as u64),
            state: crate::metrics::ProcState::Run,
        }
    }

    #[test]
    fn help_toggles_and_closes() {
        let mut app = app();
        assert!(!app.show_help);
        app.toggle_help();
        assert!(app.show_help);
        app.close_help();
        assert!(!app.show_help);
    }

    #[test]
    fn focus_cycles_through_all_panels() {
        let mut app = app();
        let start = app.focus.index();
        for _ in 0..Panel::ALL.len() {
            app.next_focus();
        }
        assert_eq!(app.focus.index(), start);
    }

    #[test]
    fn process_keys_enabled_only_on_dashboard() {
        let mut app = app();
        // Default view draws the process table, so process keys are active.
        assert_eq!(app.view, View::Dashboard);
        assert!(app.process_keys_enabled());
        app.toggle_view();
        assert_eq!(app.view, View::Graphs);
        assert!(!app.process_keys_enabled());
        app.toggle_view();
        assert_eq!(app.view, View::Investigation);
        assert!(!app.process_keys_enabled());
        // Opening the investigation directly lands in the same scoped state.
        app.toggle_view();
        app.open_investigation();
        assert_eq!(app.view, View::Investigation);
        assert!(!app.process_keys_enabled());
    }

    #[test]
    fn gpu_history_marks_gaps_and_reports_stats() {
        let mut app = app();
        app.gpu_history.clear();
        for v in [Some(10u64), None, Some(30), None, Some(50)] {
            app.gpu_history.push_back(v);
        }
        assert_eq!(
            app.gpu_points(),
            vec![(0.0, 10.0), (2.0, 30.0), (4.0, 50.0)]
        );
        assert_eq!(app.gpu_spark(), vec![10, 10, 30, 30, 50]);
        let s = app.gpu_stat();
        assert_eq!(s.current, 50);
        assert_eq!(s.avg, 30);
        assert_eq!(s.peak, 50);
        assert!(app.gpu_has_gaps());
    }

    #[test]
    fn scroll_clamps_to_content() {
        let mut app = app();
        app.snap.procs = (0..20)
            .map(|i| proc(i, &format!("p{i}"), 0.0, None))
            .collect();
        app.rebuild_proc_view();
        app.set_page(Panel::Processes, 5);
        app.scroll(100);
        assert_eq!(app.offset(Panel::Processes), 15);
        app.scroll(-100);
        assert_eq!(app.offset(Panel::Processes), 0);
        app.scroll_end();
        assert_eq!(app.offset(Panel::Processes), 15);
    }

    #[test]
    fn selection_follows_pid_across_refresh() {
        let mut app = app();
        app.snap.procs = vec![
            proc(1, "a", 30.0, None),
            proc(2, "b", 20.0, None),
            proc(3, "c", 10.0, None),
        ];
        app.rebuild_proc_view();
        // Sorted by CPU desc: a, b, c. Select "b".
        app.move_selection(1);
        assert_eq!(app.selected_pid(), Some(2));
        // A refresh changes the ordering (b now busiest); selection must still be b.
        app.snap.procs[1].cpu = 99.0;
        app.rebuild_proc_view();
        assert_eq!(app.selected_pid(), Some(2));
        assert_eq!(app.sel, 0);
    }

    #[test]
    fn selection_survives_a_process_disappearing() {
        let mut app = app();
        app.snap.procs = vec![proc(1, "a", 30.0, None), proc(2, "b", 20.0, None)];
        app.rebuild_proc_view();
        app.move_selection(1);
        assert_eq!(app.selected_pid(), Some(2));
        app.snap.procs.remove(1);
        app.rebuild_proc_view();
        // Falls back to a valid index rather than panicking.
        assert!(app.selected_pid().is_some());
    }

    #[test]
    fn query_and_filter_reduce_view_without_touching_collected_rows() {
        let mut app = app();
        app.snap.procs = vec![
            proc(1, "chrome", 50.0, None),
            proc(2, "svchost", 0.0, None),
            proc(3, "chrome", 0.0, None),
        ];
        app.rebuild_proc_view();
        assert_eq!(app.view_rows().len(), 3);
        app.begin_query();
        for c in "chrome".chars() {
            app.query_push(c);
        }
        assert_eq!(app.view_rows().len(), 2);
        // Collected rows are untouched.
        assert_eq!(app.snap.procs.len(), 3);
        app.query_clear();
        app.cycle_filter(); // -> Active
        assert_eq!(app.view_rows().len(), 1);
    }

    #[test]
    fn tree_mode_nests_and_expands() {
        let mut app = app();
        app.snap.procs = vec![proc(1, "root", 0.0, None), proc(2, "child", 0.0, Some(1))];
        app.rebuild_proc_view();
        app.toggle_tree();
        // Collapsed: only the root.
        assert_eq!(app.view_rows().len(), 1);
        app.toggle_expanded(); // expand the selected root
        assert_eq!(app.view_rows().len(), 2);
    }

    #[test]
    fn self_process_cannot_be_killed() {
        let mut app = app();
        let me = app.self_pid;
        app.snap.procs = vec![proc(me, "neomonitor", 0.0, None)];
        app.rebuild_proc_view();
        app.request_kill();
        assert!(app.pending_kill.is_none());
        assert!(app.status_text().is_some());
    }

    #[test]
    fn confirm_without_pending_is_a_noop() {
        let mut app = app();
        app.confirm_kill();
        assert!(app.pending_kill.is_none());
    }
}
