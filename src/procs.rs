//! Process explorer: pure view helpers plus an on-demand probe.
//!
//! The sampler collects a cheap [`ProcRow`] for every process on the process tick. Everything
//! in the first half of this module is **pure** and platform-independent: it turns the collected
//! rows plus the current query / filter / sort / tree state into a flat list of visible rows
//! with tree depths. Keeping that separate from collection means the UI can re-filter and
//! re-sort thousands of rows without touching the OS.
//!
//! The second half, [`ProcProbe`], owns its **own** `System` so that gathering expensive detail
//! (executable, command line, owner, working directory) and terminating a process never races
//! the sampler thread. It is only used on demand.

use std::collections::{HashMap, HashSet};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, Users};

use crate::calc;
use crate::metrics::{ProcDetail, ProcIdentity, ProcRow, ProcState};

/// Which processes the view keeps. Filtering is by cheap, always-present attributes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Filter {
    #[default]
    All,
    /// Processes currently doing work (any measurable CPU).
    Active,
    /// Processes holding a non-trivial amount of resident memory.
    Memory,
}

impl Filter {
    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Active => "active",
            Filter::Memory => "memory",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Filter::All => Filter::Active,
            Filter::Active => Filter::Memory,
            Filter::Memory => Filter::All,
        }
    }

    pub fn matches(self, row: &ProcRow) -> bool {
        match self {
            Filter::All => true,
            Filter::Active => row.cpu >= 0.1,
            Filter::Memory => row.mem >= 32 * 1024 * 1024,
        }
    }
}

/// Sort key for the process table. The metric sorts are descending (busiest first); the
/// identifier sorts are ascending.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SortKey {
    #[default]
    Cpu,
    Memory,
    Name,
    Pid,
    Start,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Cpu => "CPU",
            SortKey::Memory => "MEM",
            SortKey::Name => "NAME",
            SortKey::Pid => "PID",
            SortKey::Start => "AGE",
        }
    }

    pub fn next(self) -> Self {
        match self {
            SortKey::Cpu => SortKey::Memory,
            SortKey::Name => SortKey::Pid,
            SortKey::Pid => SortKey::Start,
            SortKey::Start => SortKey::Cpu,
            SortKey::Memory => SortKey::Name,
        }
    }
}

/// A visible row: an index into the collected rows plus its indentation depth in tree mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ViewRow {
    pub idx: usize,
    pub depth: u16,
}

/// Inputs that shape the visible process list.
pub struct ViewOptions<'a> {
    pub query: &'a str,
    pub filter: Filter,
    pub sort: SortKey,
    pub tree: bool,
    pub expanded: &'a HashSet<u32>,
}

/// Case-insensitive match against the process name, or a substring match against the PID.
/// An empty (or whitespace-only) query matches everything.
pub fn matches_query(row: &ProcRow, query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    if row.name.to_lowercase().contains(&q.to_lowercase()) {
        return true;
    }
    row.pid.to_string().contains(q)
}

/// Deterministic ordering: a primary key, then name (case-insensitive), then PID. Missing
/// start times sort last so unknown-age processes never masquerade as the newest.
pub fn cmp_rows(a: &ProcRow, b: &ProcRow, key: SortKey) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let primary = match key {
        SortKey::Cpu => b.cpu.partial_cmp(&a.cpu).unwrap_or(Ordering::Equal),
        SortKey::Memory => b.mem.cmp(&a.mem),
        SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        SortKey::Pid => a.pid.cmp(&b.pid),
        SortKey::Start => match (a.start_time, b.start_time) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        },
    };
    primary
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| a.pid.cmp(&b.pid))
}

/// Indices of `rows` that pass the query and filter, in deterministic sort order.
pub fn ordered_indices(rows: &[ProcRow], opts: &ViewOptions) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..rows.len())
        .filter(|&i| matches_query(&rows[i], opts.query) && opts.filter.matches(&rows[i]))
        .collect();
    idx.sort_by(|&a, &b| cmp_rows(&rows[a], &rows[b], opts.sort));
    idx
}

/// Build the flat visible list, applying the filter, sort and (optionally) the process tree.
///
/// In tree mode roots are processes with no live parent in the filtered set. Only a node that is
/// in `expanded` shows its children. Processes left unreachable by a parent/child cycle are
/// still emitted at depth 0 so nothing silently disappears.
pub fn build_view(rows: &[ProcRow], opts: &ViewOptions) -> Vec<ViewRow> {
    let order = ordered_indices(rows, opts);
    if !opts.tree {
        return order
            .into_iter()
            .map(|idx| ViewRow { idx, depth: 0 })
            .collect();
    }

    let present: HashSet<u32> = order.iter().map(|&i| rows[i].pid).collect();
    // Children grouped by parent PID, preserving the sorted order of `order`.
    let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
    // Parent pointer (only for live parents) so we can tell "hidden under a collapsed node"
    // apart from "unreachable because of a cycle".
    let mut parent_of: HashMap<u32, u32> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for &i in &order {
        let row = &rows[i];
        match row.ppid {
            Some(pp) if pp != row.pid && present.contains(&pp) => {
                children.entry(pp).or_default().push(i);
                parent_of.insert(row.pid, pp);
            }
            // No parent, self-parent, or parent filtered out: treat as a root.
            _ => roots.push(i),
        }
    }

    // Whether following parent links from `start` terminates at a root. Returns false when the
    // walk enters a cycle, which marks the node as genuinely unreachable.
    fn reaches_root(start: u32, parent_of: &HashMap<u32, u32>) -> bool {
        let mut seen = HashSet::new();
        let mut cur = start;
        loop {
            if !seen.insert(cur) {
                return false;
            }
            match parent_of.get(&cur) {
                Some(&p) => cur = p,
                None => return true,
            }
        }
    }

    let mut out: Vec<ViewRow> = Vec::with_capacity(order.len());
    let mut visited: Vec<bool> = vec![false; rows.len()];

    fn add(
        root: usize,
        rows: &[ProcRow],
        children: &HashMap<u32, Vec<usize>>,
        expanded: &HashSet<u32>,
        visited: &mut [bool],
        out: &mut Vec<ViewRow>,
    ) {
        // Iterative DFS: a deep parent/child chain must not grow the call stack, and a cycle is
        // already prevented by `visited`, so no artificial depth cap is needed.
        let mut stack: Vec<(usize, u16)> = vec![(root, 0)];
        while let Some((idx, depth)) = stack.pop() {
            if visited[idx] {
                continue;
            }
            visited[idx] = true;
            out.push(ViewRow { idx, depth });
            if expanded.contains(&rows[idx].pid)
                && let Some(kids) = children.get(&rows[idx].pid)
            {
                for &child in kids.iter().rev() {
                    if !visited[child] {
                        stack.push((child, depth.saturating_add(1)));
                    }
                }
            }
        }
    }

    for &root in &roots {
        add(root, rows, &children, opts.expanded, &mut visited, &mut out);
    }
    // Nodes left unreachable by a parent/child cycle: emit each as a root so nothing silently
    // disappears. Nodes merely hidden under a collapsed ancestor are skipped.
    for &i in &order {
        if !visited[i] && !reaches_root(rows[i].pid, &parent_of) {
            add(i, rows, &children, opts.expanded, &mut visited, &mut out);
        }
    }
    out
}

/// Result of a termination request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TerminateOutcome {
    /// The kill request was accepted by the OS.
    Sent,
    /// The OS rejected the request (typically insufficient privileges).
    Failed,
    /// The process had already exited before the request.
    Gone,
    /// The selected identity no longer matches (e.g. the PID was reused); nothing was done.
    Refused(&'static str),
}

impl TerminateOutcome {
    pub fn message(self) -> &'static str {
        match self {
            TerminateOutcome::Sent => "terminate request sent",
            TerminateOutcome::Failed => "terminate failed (access denied?)",
            TerminateOutcome::Gone => "process already exited",
            TerminateOutcome::Refused(reason) => reason,
        }
    }
}

/// On-demand, single-process OS access. Kept out of the sampler so a slow detail refresh or a
/// termination never disturbs the collection cadence.
pub struct ProcProbe {
    sys: System,
    users: Users,
}

impl Default for ProcProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcProbe {
    pub fn new() -> Self {
        Self {
            sys: System::new(),
            users: Users::new_with_refreshed_list(),
        }
    }

    fn refresh_one(&mut self, pid: Pid, kind: ProcessRefreshKind) {
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, kind);
    }

    fn state_of(p: &sysinfo::Process) -> ProcState {
        match p.status() {
            sysinfo::ProcessStatus::Idle => ProcState::Idle,
            sysinfo::ProcessStatus::Run => ProcState::Run,
            sysinfo::ProcessStatus::Sleep => ProcState::Sleep,
            sysinfo::ProcessStatus::Stop => ProcState::Stop,
            sysinfo::ProcessStatus::Zombie => ProcState::Zombie,
            sysinfo::ProcessStatus::Tracing => ProcState::Tracing,
            sysinfo::ProcessStatus::Dead => ProcState::Dead,
            sysinfo::ProcessStatus::Wakekill => ProcState::Wakekill,
            sysinfo::ProcessStatus::Waking => ProcState::Waking,
            sysinfo::ProcessStatus::Parked => ProcState::Parked,
            sysinfo::ProcessStatus::LockBlocked => ProcState::LockBlocked,
            sysinfo::ProcessStatus::UninterruptibleDiskSleep => ProcState::DiskSleep,
            sysinfo::ProcessStatus::Suspended => ProcState::Suspended,
            _ => ProcState::Unknown,
        }
    }

    /// Gather the expensive detail of a single process. `logical` is the logical CPU count used
    /// to normalise CPU usage to 0–100 %.
    pub fn detail(&mut self, pid: u32, logical: usize) -> Option<ProcDetail> {
        let spid = Pid::from_u32(pid);
        self.refresh_one(spid, ProcessRefreshKind::everything().without_tasks());
        let p = self.sys.process(spid)?;
        let user = p
            .user_id()
            .and_then(|u| self.users.get_user_by_id(u))
            .map(|u| u.name().to_string());
        let cmd = {
            let parts: Vec<String> = p
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" "))
            }
        };
        let disk = p.disk_usage();
        let start = p.start_time();
        Some(ProcDetail {
            pid,
            name: p.name().to_string_lossy().into_owned(),
            exe: p.exe().map(|e| e.to_string_lossy().into_owned()),
            cmd,
            user,
            cwd: p.cwd().map(|c| c.to_string_lossy().into_owned()),
            state: Self::state_of(p),
            ppid: p.parent().map(|p| p.as_u32()),
            start_time: (start != 0).then_some(start),
            run_time: p.run_time(),
            cpu: calc::process_cpu_percent(p.cpu_usage(), logical),
            mem: p.memory(),
            virtual_mem: p.virtual_memory(),
            disk_read: disk.total_read_bytes,
            disk_written: disk.total_written_bytes,
            env_count: p.environ().len(),
        })
    }

    /// Terminate a process after revalidating its identity. Returns [`TerminateOutcome::Refused`]
    /// when the PID no longer refers to the selected process, so a reused PID is never killed.
    ///
    /// On Windows sysinfo only performs a **forceful** termination (`taskkill /F`); a graceful
    /// request is not available, which is why the caller must confirm explicitly.
    pub fn terminate(&mut self, ident: &ProcIdentity) -> TerminateOutcome {
        let spid = Pid::from_u32(ident.pid);
        self.refresh_one(spid, ProcessRefreshKind::nothing());
        match self.sys.process(spid) {
            None => TerminateOutcome::Gone,
            Some(p) => {
                let name = p.name().to_string_lossy().into_owned();
                let st = p.start_time();
                let st = (st != 0).then_some(st);
                if !ident.matches(ident.pid, &name, st) {
                    return TerminateOutcome::Refused("refused: PID was reused by another process");
                }
                if p.kill() {
                    TerminateOutcome::Sent
                } else {
                    TerminateOutcome::Failed
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;

    fn row(pid: u32, name: &str, cpu: f32, mem: u64, ppid: Option<u32>) -> ProcRow {
        ProcRow {
            pid,
            name: name.to_string(),
            cpu,
            mem,
            ppid,
            start_time: Some(pid as u64 * 100),
            state: ProcState::Run,
        }
    }

    static EMPTY: LazyLock<HashSet<u32>> = LazyLock::new(HashSet::new);

    fn opts(query: &str, filter: Filter, sort: SortKey) -> ViewOptions<'_> {
        ViewOptions {
            query,
            filter,
            sort,
            tree: false,
            expanded: &EMPTY,
        }
    }

    #[test]
    fn empty_list_yields_empty_view() {
        let v = build_view(&[], &opts("", Filter::All, SortKey::Cpu));
        assert!(v.is_empty());
    }

    #[test]
    fn query_matches_name_and_pid_case_insensitively() {
        let rows = [
            row(1, "Explorer.exe", 0.0, 0, None),
            row(4321, "svchost", 0.0, 0, None),
        ];
        let v = build_view(&rows, &opts("explo", Filter::All, SortKey::Cpu));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].idx, 0);
        // PID substring.
        let v = build_view(&rows, &opts("432", Filter::All, SortKey::Cpu));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].idx, 1);
        // Whitespace-only query matches everything.
        assert_eq!(
            build_view(&rows, &opts("   ", Filter::All, SortKey::Cpu)).len(),
            2
        );
    }

    #[test]
    fn filter_selects_active_and_memory() {
        let rows = [
            row(1, "idle", 0.0, 1024, None),
            row(2, "busy", 5.0, 1024, None),
            row(3, "fat", 0.0, 64 * 1024 * 1024, None),
        ];
        let active = build_view(&rows, &opts("", Filter::Active, SortKey::Cpu));
        assert_eq!(active.len(), 1);
        assert_eq!(rows[active[0].idx].name, "busy");
        let mem = build_view(&rows, &opts("", Filter::Memory, SortKey::Cpu));
        assert_eq!(mem.len(), 1);
        assert_eq!(rows[mem[0].idx].name, "fat");
    }

    #[test]
    fn sort_is_deterministic_with_tie_breaks() {
        // Equal CPU: name asc, then pid asc.
        let rows = [
            row(30, "bbb", 10.0, 0, None),
            row(20, "aaa", 10.0, 0, None),
            row(10, "aaa", 10.0, 0, None),
        ];
        let v = build_view(&rows, &opts("", Filter::All, SortKey::Cpu));
        let names: Vec<u32> = v.iter().map(|r| rows[r.idx].pid).collect();
        assert_eq!(names, vec![10, 20, 30]);
    }

    #[test]
    fn compare_handles_nan_and_missing_start_time() {
        let mut a = row(1, "a", f32::NAN, 0, None);
        a.start_time = None;
        let b = row(2, "b", 1.0, 0, None);
        // Must not panic on NaN, and missing start times sort last.
        let _ = cmp_rows(&a, &b, SortKey::Cpu);
        assert_eq!(
            cmp_rows(&a, &b, SortKey::Start),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn tree_nests_children_under_expanded_parent() {
        let rows = [
            row(1, "root", 0.0, 0, None),
            row(2, "child", 0.0, 0, Some(1)),
            row(3, "grand", 0.0, 0, Some(2)),
        ];
        // Collapsed: only the root.
        let mut v = build_view(
            &rows,
            &ViewOptions {
                query: "",
                filter: Filter::All,
                sort: SortKey::Pid,
                tree: true,
                expanded: &HashSet::new(),
            },
        );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].depth, 0);
        // Expand root only: root + child, grandchild still hidden.
        let exp: HashSet<u32> = [1].into_iter().collect();
        v = build_view(
            &rows,
            &ViewOptions {
                query: "",
                filter: Filter::All,
                sort: SortKey::Pid,
                tree: true,
                expanded: &exp,
            },
        );
        assert_eq!(
            v.iter()
                .map(|r| (rows[r.idx].pid, r.depth))
                .collect::<Vec<_>>(),
            vec![(1, 0), (2, 1)]
        );
    }

    #[test]
    fn tree_handles_missing_parent_and_cycles() {
        let rows = [
            row(1, "orphan", 0.0, 0, Some(999)),
            row(2, "self", 0.0, 0, Some(2)),
            row(3, "a", 0.0, 0, Some(4)),
            row(4, "b", 0.0, 0, Some(3)),
        ];
        let exp: HashSet<u32> = [1, 2, 3, 4].into_iter().collect();
        let v = build_view(
            &rows,
            &ViewOptions {
                query: "",
                filter: Filter::All,
                sort: SortKey::Pid,
                tree: true,
                expanded: &exp,
            },
        );
        // Every process appears exactly once; the cycle is broken without hanging.
        assert_eq!(v.len(), 4);
        let mut seen: Vec<u32> = v.iter().map(|r| rows[r.idx].pid).collect();
        seen.sort();
        assert_eq!(seen, vec![1, 2, 3, 4]);
    }

    #[test]
    fn tree_on_large_list_is_linear_and_complete() {
        // 5000-node chain: recursion must be bounded and every node emitted.
        let mut rows = vec![row(0, "root", 0.0, 0, None)];
        for pid in 1..5000u32 {
            rows.push(row(pid, "p", 0.0, 0, Some(pid - 1)));
        }
        let exp: HashSet<u32> = (0..5000u32).collect();
        let v = build_view(
            &rows,
            &ViewOptions {
                query: "",
                filter: Filter::All,
                sort: SortKey::Pid,
                tree: true,
                expanded: &exp,
            },
        );
        assert_eq!(v.len(), 5000);
    }

    #[test]
    fn identity_rejects_reused_pid() {
        let ident = ProcIdentity {
            pid: 42,
            name: "old".to_string(),
            start_time: Some(1000),
        };
        assert!(ident.matches(42, "old", Some(1000)));
        // Same PID, different start time -> reused.
        assert!(!ident.matches(42, "new", Some(2000)));
        // Same name but missing start time on one side -> accepted by fallback.
        assert!(ident.matches(42, "old", None));
        assert!(!ident.matches(43, "old", Some(1000)));
    }
}
