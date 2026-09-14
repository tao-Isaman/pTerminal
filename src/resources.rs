use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct ProcSample {
    pub pid: u32,
    pub parent: Option<u32>,
    pub cpu: f32,
    pub mem: u64,
    /// Executable name as reported by the OS (e.g. `python.exe`), consumed
    /// by [`worker_procs`]' grouping.
    pub name: String,
}

/// [`worker_procs`]' denylist: what the agent runtime chain itself is made
/// of on this platform (`cmd /c claude` → node, plus conhost noise) — never
/// "workers" worth a row of their own.
// ponytail: a name denylist, not process-lineage analysis; a genuine
// background node.exe worker would be hidden. Revisit only if that happens.
const RUNTIME_PROCS: [&str; 4] = ["cmd", "conhost", "node", "claude"];

/// Groups the live descendant processes of `roots` (the tab's claimed PID
/// tree) by executable name — lowercased, `.exe` stripped, minus
/// [`RUNTIME_PROCS`] — into `(name, count)` rows, most numerous first (ties
/// by name). This is what lets an agent tab show `4x python` while a script
/// with parallel workers runs, instead of nothing at all: those workers are
/// real child processes but never Claude subagents, so no hook event will
/// ever report them.
pub fn worker_procs(roots: &[u32], procs: &[ProcSample]) -> Vec<(String, usize)> {
    let ds = descendants(roots, procs);
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for p in procs.iter().filter(|p| ds.contains(&p.pid)) {
        let name = p.name.to_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        if RUNTIME_PROCS.contains(&name) { continue; }
        *counts.entry(name.to_string()).or_default() += 1;
    }
    let mut out: Vec<(String, usize)> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

#[derive(Clone, Debug, Default)]
pub struct MachineStats {
    pub mem_total: u64,
    pub mem_used: u64,
    pub cpu_pct: f32,
}

fn descendants(roots: &[u32], procs: &[ProcSample]) -> HashSet<u32> {
    let mut set: HashSet<u32> = roots.iter().copied().collect();
    loop {
        let before = set.len();
        for p in procs {
            if let Some(pp) = p.parent {
                if set.contains(&pp) { set.insert(p.pid); }
            }
        }
        if set.len() == before { return set; }
    }
}

pub fn rollup(roots: &[u32], procs: &[ProcSample]) -> (f32, u64) {
    let ds = descendants(roots, procs);
    procs.iter()
        .filter(|p| ds.contains(&p.pid))
        .fold((0.0, 0), |(c, m), p| (c + p.cpu, m + p.mem))
}

pub fn new_children(before: &HashSet<u32>, procs: &[ProcSample], parent: u32) -> Vec<u32> {
    procs.iter()
        .filter(|p| p.parent == Some(parent) && !before.contains(&p.pid))
        .map(|p| p.pid)
        .collect()
}

/// Exactly the per-process facts [`ProcSample`] carries, and nothing else.
///
/// The obvious call, `System::refresh_processes(All, true)`, is *not* free: it
/// expands to `ProcessRefreshKind::nothing().with_memory().with_cpu()
/// .with_disk_usage().with_exe(UpdateKind::OnlyIfNotSet).with_tasks()`. Two of
/// those we never read, and we were paying for them every 2s forever:
///
///   * `with_disk_usage()` → a `GetProcessIoCounters` call per process, every
///     cycle, for read/write byte counters no part of pTerminal has ever
///     displayed. This is the bulk of the steady-state saving.
///   * `with_exe(OnlyIfNotSet)` → `QueryFullProcessImageNameW` per process
///     whose exe path is still unknown, plus a retained `PathBuf` per process
///     that resolved. `OnlyIfNotSet` sounds self-limiting but isn't: the many
///     protected/system processes we can never open a handle for never get an
///     exe, so they are re-attempted on *every* cycle forever. Once the cache
///     is warm this is near-free, but it is what produced the old path's long
///     tail whenever the machine churned processes. We show `p.name()`, which
///     comes from the `PROCESSENTRY32W` toolhelp snapshot entry and is
///     populated at process-discovery time regardless of `ProcessRefreshKind`
///     — the exe path was never feeding the UI at all.
///   * `with_tasks()` is a no-op on Windows (the win32 backend never reads the
///     flag), but on Linux it makes sysinfo enumerate every *thread* as a
///     process. That would be actively wrong for [`worker_procs`], which
///     counts processes, so turn it off rather than rely on the platform.
///
/// Measured on the dev box (~515 processes, debug build, paired A/B alternating
/// the two refresh kinds on one `System` so machine noise hits both arms
/// equally, n=70 pairs): old median 34.8ms/cycle vs new 31.4ms — a median
/// saving of 3.7ms per 2s cycle, holding at ~3.4ms at p25/p75 and at the
/// minimum. The tail improves much more than the median: worst observed cycle
/// 350ms old vs 77ms new, and on a cold `System` (empty exe cache, the
/// pathological case) the old kind was measured at a 120ms median with a
/// 1080ms worst case.
///
/// What is left is irreducible given what the UI shows: of a ~31ms cycle,
/// ~12ms is the toolhelp enumeration we cannot narrow (see the note on
/// `ProcessesToUpdate::All` below), ~11ms is `with_cpu`'s per-process
/// `GetProcessTimes`, and ~1.5ms is `with_memory`. Both of those last two are
/// read by `rollup`, so neither can go.
///
/// `pid`, `parent` and `name` need no flag at all — all three come straight off
/// the toolhelp snapshot entry when the process is discovered.
fn proc_refresh_kind() -> sysinfo::ProcessRefreshKind {
    sysinfo::ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .without_tasks()
}

pub fn spawn_sampler() -> std::sync::mpsc::Receiver<(Vec<ProcSample>, MachineStats)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sys = sysinfo::System::new();
        let proc_kind = proc_refresh_kind();
        // We read `total_memory`/`used_memory` (physical RAM) and nothing else.
        // Plain `refresh_memory()` is `MemoryRefreshKind::everything()`, which
        // additionally runs `K32GetPerformanceInfo` for swap totals we never
        // display. It is only tens of microseconds — a rounding error next to
        // the process sweep — but it is pure waste and costs nothing to drop.
        let mem_kind = sysinfo::MemoryRefreshKind::nothing().with_ram();

        // Warm up CPU measurements: sysinfo requires two refreshes separated by an interval
        // to calculate accurate CPU usage. The first refresh populates baseline, the second
        // measures the delta. Without this warmup, the first snapshot contains ~0/garbage
        // CPU values. We use a conservative 200ms interval.
        sys.refresh_cpu_usage();
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, proc_kind);
        std::thread::sleep(std::time::Duration::from_millis(200));

        loop {
            // `ProcessesToUpdate::All` is load-bearing and must stay: `descendants`
            // walks the *whole* process table to find children of a tab's claimed
            // root PIDs, so a worker process missing from the refreshed set is a
            // worker the sidebar cannot see. The saving here is deliberately in
            // how much we learn about each process, never in how many we look at.
            //
            // `remove_dead_processes: true` (the second argument) is what keeps
            // `sys` from growing without bound: combined with `All`, sysinfo
            // `retain`s only processes seen in this sweep, so exited processes and
            // their per-process allocations are dropped the cycle they die. With
            // exe paths no longer fetched, the only thing retained per live
            // process is now the name plus a cached handle.
            sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, proc_kind);
            sys.refresh_memory_specifics(mem_kind);
            // Already minimal: `refresh_cpu_usage()` is
            // `CpuRefreshKind::nothing().with_cpu_usage()`, i.e. no frequency
            // probing. We only read `global_cpu_usage()`, but the Windows backend
            // has no global-only path — one PDH query refreshes the `_Total`
            // counter and the per-core counters together, and its cost scales
            // with core count, not process count (~0.1ms steady state; the first
            // call pays a one-off ~300ms to register the PDH counters).
            sys.refresh_cpu_usage();
            let snap: Vec<ProcSample> = sys.processes().iter().map(|(pid, p)| ProcSample {
                pid: pid.as_u32(),
                parent: p.parent().map(|pp| pp.as_u32()),
                cpu: p.cpu_usage(),
                mem: p.memory(),
                name: p.name().to_string_lossy().into_owned(),
            }).collect();
            let machine = MachineStats {
                mem_total: sys.total_memory(),
                mem_used: sys.used_memory(),
                cpu_pct: sys.global_cpu_usage(),
            };
            if tx.send((snap, machine)).is_err() { return; } // app gone, thread exits
            // ponytail: a fixed 2s cadence, deliberately. Backing off when no
            // agent tab is running would save far more than trimming the
            // refresh kind does (the whole ~31ms sweep instead of ~3.7ms of it),
            // but this thread has no idea what tabs exist — `spawn_sampler`
            // takes no arguments and hands back only a `Receiver`, so telling
            // it would mean changing the signature and threading a shared flag
            // out of `App`. Left alone on purpose to keep this change confined
            // to resources.rs. Note also that the machine-stats row and the
            // per-tab memory column are drawn from this snapshot even with
            // zero agent tabs, so any backoff must keep sampling, just slower.
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn p(pid: u32, parent: Option<u32>, cpu: f32, mem: u64) -> ProcSample {
        ProcSample { pid, parent, cpu, mem, name: String::new() }
    }

    fn pn(pid: u32, parent: Option<u32>, name: &str) -> ProcSample {
        ProcSample { pid, parent, cpu: 0.0, mem: 0, name: name.to_string() }
    }

    #[test]
    fn rollup_sums_descendant_tree() {
        // 10 -> 20 -> 30, and unrelated 99
        let procs = vec![p(10, None, 1.0, 100), p(20, Some(10), 2.0, 200),
                         p(30, Some(20), 4.0, 400), p(99, None, 8.0, 800)];
        let (cpu, mem) = rollup(&[10], &procs);
        assert_eq!(cpu, 7.0);
        assert_eq!(mem, 700);
    }

    #[test]
    fn rollup_multiple_roots_no_double_count() {
        let procs = vec![p(10, None, 1.0, 100), p(20, Some(10), 2.0, 200)];
        let (cpu, mem) = rollup(&[10, 20], &procs);
        assert_eq!(cpu, 3.0);
        assert_eq!(mem, 300);
    }

    #[test]
    fn finds_new_children_only() {
        let before: HashSet<u32> = [20u32].into_iter().collect();
        let procs = vec![p(20, Some(1), 0.0, 0), p(21, Some(1), 0.0, 0), p(22, Some(2), 0.0, 0)];
        assert_eq!(new_children(&before, &procs, 1), vec![21]);
    }

    /// The agent-14 shape: a script fans out worker processes under the
    /// tab's runtime chain. The chain itself (cmd → claude, conhost noise)
    /// must not show; the workers must, grouped and counted, name
    /// normalized (case, `.exe`); processes outside the tab's tree don't
    /// count.
    #[test]
    fn worker_procs_groups_descendants_excluding_runtime() {
        let procs = vec![
            pn(10, None, "cmd.exe"),
            pn(20, Some(10), "claude.exe"),
            pn(30, Some(20), "python.exe"),
            pn(31, Some(20), "Python.EXE"),
            pn(32, Some(20), "python.exe"),
            pn(33, Some(20), "python.exe"),
            pn(40, Some(20), "conhost.exe"),
            pn(99, None, "python.exe"), // not a descendant of the tab
        ];
        assert_eq!(worker_procs(&[10], &procs), vec![("python".to_string(), 4)]);
    }

    #[test]
    fn worker_procs_sorts_by_count_then_name() {
        let procs = vec![
            pn(10, None, "cmd.exe"),
            pn(20, Some(10), "node.exe"), // runtime, hidden
            pn(21, Some(20), "ffmpeg.exe"),
            pn(22, Some(20), "python.exe"),
            pn(23, Some(20), "python.exe"),
            pn(24, Some(20), "bun.exe"),
        ];
        assert_eq!(
            worker_procs(&[10], &procs),
            vec![
                ("python".to_string(), 2),
                ("bun".to_string(), 1),
                ("ffmpeg".to_string(), 1),
            ]
        );
    }

    /// Pins the sampler's [`ProcessRefreshKind`] to exactly the fields
    /// [`ProcSample`] carries. This is a real regression guard, not a
    /// benchmark: the cheap way to "fix" a future bug here is to reach for
    /// `refresh_processes(All, true)` again, which silently re-enables the
    /// per-process disk-usage and exe-path syscalls this module went out of
    /// its way to stop paying for. Failing loudly is better than quietly
    /// regressing background CPU that nothing else measures.
    ///
    /// The inverse also matters: dropping `cpu` or `memory` here would leave
    /// `rollup` summing zeroes and the sidebar showing `0%` / `0 MB` forever
    /// without any other test noticing.
    #[test]
    fn refresh_kind_matches_procsample_fields() {
        use sysinfo::UpdateKind;
        let k = proc_refresh_kind();
        assert!(k.cpu(), "ProcSample::cpu feeds rollup and the sidebar CPU column");
        assert!(k.memory(), "ProcSample::mem feeds rollup and the sidebar memory column");
        assert!(!k.disk_usage(), "no consumer reads DiskUsage");
        assert_eq!(k.exe(), UpdateKind::Never, "worker_procs uses name(), never exe()");
        assert_eq!(k.cmd(), UpdateKind::Never);
        assert_eq!(k.environ(), UpdateKind::Never);
        assert_eq!(k.user(), UpdateKind::Never);
        assert_eq!(k.cwd(), UpdateKind::Never);
        assert_eq!(k.root(), UpdateKind::Never);
        assert!(!k.tasks(), "worker_procs counts processes, not threads");
    }

    /// End-to-end proof that the trimmed refresh still populates every field
    /// each consumer reads. Narrowing a `ProcessRefreshKind` fails silently —
    /// a wrong flag yields `cpu: 0.0` / `mem: 0` / `name: ""` rather than an
    /// error — so this asserts on live data from the real sampler thread:
    ///
    ///   * `pid` + `parent` → `descendants`, `new_children`
    ///   * `mem` + `cpu`    → `rollup`
    ///   * `name`           → `worker_procs`
    ///
    /// CPU is checked across successive snapshots rather than the first: the
    /// first snapshot lands right at sysinfo's 200ms `MINIMUM_CPU_UPDATE_INTERVAL`
    /// boundary, so its usage figures may still be the warm-up baseline of 0.
    #[test]
    fn sampler_produces_snapshots() {
        let rx = spawn_sampler();
        let me = std::process::id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut saw_cpu = false;
        loop {
            let (snap, machine) =
                rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            assert!(machine.mem_total > 0);
            assert!(machine.mem_used > 0);

            // pid: we see ourselves.
            let mine = snap.iter().find(|s| s.pid == me).expect("own pid missing from snapshot");
            // name: `worker_procs` lowercases and strips `.exe` off this; an
            // empty string would make every worker row collapse into one blank
            // group instead of `4x python`.
            assert!(!mine.name.is_empty(), "ProcSample::name is empty");
            // mem: `rollup`'s second component.
            assert!(mine.mem > 0, "ProcSample::mem is zero for a live process");
            // parent: `descendants` and `new_children` are pure no-ops without it.
            assert!(snap.iter().any(|s| s.parent == Some(me) || s.parent.is_some()),
                    "no process in the snapshot reports a parent");

            // cpu: `rollup`'s first component. Machine-wide, not our own — a
            // sleeping test process can legitimately report 0.0%.
            if snap.iter().map(|s| s.cpu).sum::<f32>() > 0.0 {
                saw_cpu = true;
                break;
            }
            assert!(std::time::Instant::now() < deadline, "timed out waiting for CPU data");
        }
        assert!(saw_cpu, "ProcSample::cpu is zero for every process on the machine");
    }
}
