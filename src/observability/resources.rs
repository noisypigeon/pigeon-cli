use std::collections::HashSet;
use std::time::Duration;

use sysinfo::{Pid, Process, ProcessesToUpdate, System};

/// Periodically samples this process's CPU/memory/disk-I/O usage into the
/// tracing sink while a job runs (ADR-0073) -- spawned only inside a job's
/// own tokio runtime (`email_sync`/`decrypt_files`'s `dispatch_async`), never
/// for keyring commands, which finish too fast for periodic sampling to
/// observe anything. Dropping it aborts the sampling task, so every one of a
/// job's many early-return paths cleans it up automatically, with no manual
/// bookkeeping at each call site.
pub(crate) struct ResourceSampler {
    handle: tokio::task::JoinHandle<()>,
}

/// Every live descendant of `root` (including `root` itself), found by
/// walking `Process::parent()` to a fixed point over the whole process table
/// (ADR-0120) -- a job's actual CPU/memory/disk load mostly lives in the
/// `ffmpeg`/`rclone` child processes it spawns via `tokio::process::Command`
/// (`transform/media.rs`, `transform/push.rs`, `rclone_transfer.rs`, ...),
/// invisible to a query scoped to the `pigeon` PID alone. No child-PID
/// plumbing is threaded through each spawn site -- this stays entirely
/// self-contained in the sampler, same "no manual bookkeeping at each call
/// site" posture as this struct's own doc comment above.
fn pigeon_process_tree(system: &System, root: Pid) -> Vec<&Process> {
    let mut ids: HashSet<Pid> = HashSet::from([root]);
    loop {
        let before = ids.len();
        for (pid, process) in system.processes() {
            if !ids.contains(pid) && process.parent().is_some_and(|parent| ids.contains(&parent)) {
                ids.insert(*pid);
            }
        }
        if ids.len() == before {
            break;
        }
    }
    ids.into_iter()
        .filter_map(|pid| system.process(pid))
        .collect()
}

impl ResourceSampler {
    pub(crate) fn spawn(interval: Duration) -> Self {
        let handle = tokio::spawn(async move {
            let Ok(pid) = sysinfo::get_current_pid() else {
                return;
            };
            let mut system = System::new();
            loop {
                tokio::time::sleep(interval).await;
                // A full refresh (every live PID, not just `pigeon`'s own)
                // is required to see `ffmpeg`/`rclone` children at all --
                // accepted as a conscious tradeoff at this 5s-or-slower
                // cadence (ADR-0120).
                system.refresh_processes(ProcessesToUpdate::All, true);
                let processes = pigeon_process_tree(&system, pid);
                if processes.is_empty() {
                    continue;
                }
                let cpu_percent: f64 = processes.iter().map(|p| p.cpu_usage() as f64).sum();
                let mem_bytes: f64 = processes.iter().map(|p| p.memory() as f64).sum();
                let (read_bytes, written_bytes) =
                    processes
                        .iter()
                        .fold((0u64, 0u64), |(read, written), process| {
                            let disk = process.disk_usage();
                            (read + disk.read_bytes, written + disk.written_bytes)
                        });
                // Metrics only, no log line (ADR-0093) -- a periodic
                // measurement isn't a diagnosis, and this fired every 5s for
                // a run's whole lifetime, drowning out genuine error/failure
                // logs for no benefit once a real Prometheus scrape exists.
                // CPU/memory are instantaneous gauges (`.set`), summed across
                // the whole process tree each tick. Disk I/O uses
                // `read_bytes`/`written_bytes` -- `sysinfo`'s own
                // since-last-refresh per-process deltas, not the cumulative
                // `total_*_bytes` fields -- applied via `.increment()`
                // (ADR-0120): summing *cumulative* counters with `.absolute()`
                // across a tree whose membership changes every tick (a
                // short-lived `ffmpeg` process exits, another starts) would
                // let the counter plateau or double-count; incrementing each
                // tick's own deltas stays correct regardless of which
                // processes existed at the previous tick.
                let instance = crate::observability::instance();
                metrics::gauge!("pigeon_resource_cpu_percent", "instance" => instance)
                    .set(cpu_percent);
                metrics::gauge!("pigeon_resource_mem_bytes", "instance" => instance).set(mem_bytes);
                metrics::counter!("pigeon_resource_disk_read_bytes_total", "instance" => instance)
                    .increment(read_bytes);
                metrics::counter!("pigeon_resource_disk_written_bytes_total", "instance" => instance)
                    .increment(written_bytes);
            }
        });
        Self { handle }
    }
}

impl Drop for ResourceSampler {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dropping_the_sampler_stops_its_sampling_task() {
        let sampler = ResourceSampler::spawn(Duration::from_millis(5));
        tokio::time::sleep(Duration::from_millis(20)).await;
        let abort_handle = sampler.handle.abort_handle();
        drop(sampler);
        // `abort()` cancels at the task's next await point, not instantly --
        // give it slack before asserting.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(abort_handle.is_finished());
    }

    /// Confirms `pigeon_process_tree` actually walks down to a real spawned
    /// child, not just the root PID -- the direct regression test for the
    /// bug this module fixes (CPU/memory sampling blind to `ffmpeg`/`rclone`
    /// children).
    #[tokio::test]
    async fn pigeon_process_tree_includes_a_spawned_child_process() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .expect("failed to spawn test child process");
        let child_pid = Pid::from_u32(child.id().expect("spawned child has a pid"));
        let root_pid = sysinfo::get_current_pid().expect("failed to get current pid");

        // Give the new process a moment to actually appear in the process
        // table before refreshing.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);

        let tree = pigeon_process_tree(&system, root_pid);
        let tree_pids: HashSet<Pid> = tree.iter().map(|process| process.pid()).collect();

        let _ = child.kill().await;
        let _ = child.wait().await;

        assert!(tree_pids.contains(&root_pid));
        assert!(tree_pids.contains(&child_pid));
    }
}
