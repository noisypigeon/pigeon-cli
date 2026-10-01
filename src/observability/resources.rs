use std::time::Duration;

use sysinfo::{ProcessesToUpdate, System};

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

impl ResourceSampler {
    pub(crate) fn spawn(interval: Duration) -> Self {
        let handle = tokio::spawn(async move {
            let Ok(pid) = sysinfo::get_current_pid() else {
                return;
            };
            let mut system = System::new();
            loop {
                tokio::time::sleep(interval).await;
                system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
                let Some(process) = system.process(pid) else {
                    continue;
                };
                let disk = process.disk_usage();
                tracing::info!(
                    kind = "resource_sample",
                    cpu_percent = process.cpu_usage(),
                    mem_bytes = process.memory(),
                    disk_read_bytes = disk.total_read_bytes,
                    disk_written_bytes = disk.total_written_bytes,
                    "resource sample"
                );
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
}
