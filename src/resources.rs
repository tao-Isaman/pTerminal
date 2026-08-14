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

pub fn spawn_sampler() -> std::sync::mpsc::Receiver<(Vec<ProcSample>, MachineStats)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sys = sysinfo::System::new();

        // Warm up CPU measurements: sysinfo requires two refreshes separated by an interval
        // to calculate accurate CPU usage. The first refresh populates baseline, the second
        // measures the delta. Without this warmup, the first snapshot contains ~0/garbage
        // CPU values. We use a conservative 200ms interval.
        sys.refresh_cpu_usage();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        std::thread::sleep(std::time::Duration::from_millis(200));

        loop {
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            sys.refresh_memory();
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

    #[test]
    fn sampler_produces_snapshots() {
        let rx = spawn_sampler();
        let (snap, machine) = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert!(snap.iter().any(|s| s.pid == std::process::id())); // we see ourselves
        assert!(machine.mem_total > 0);
        assert!(machine.mem_used > 0);
    }
}
