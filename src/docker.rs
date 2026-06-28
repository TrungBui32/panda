use bollard::Docker;
use bollard::container::{
    KillContainerOptions, ListContainersOptions, LogsOptions, RemoveContainerOptions,
    RestartContainerOptions, StartContainerOptions, StatsOptions, StopContainerOptions,
};
use futures::StreamExt;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct DockerContainer {
    pub id: String,        // full id (used for actions)
    pub name: String,      // friendly name, leading '/' stripped
    pub image: String,
    pub state: String,     // running / exited / paused / created ...
    pub status: String,    // e.g. "Up 3 minutes"
    pub cpu_usage: f64,    // docker-style %, can exceed 100 on multi-core
    pub mem_mb: f64,
    pub mem_limit_mb: f64,
}

#[derive(Debug, Clone, Default)]
pub struct DockerSnapshot {
    pub available: bool,
    pub error: Option<String>,
    pub containers: Vec<DockerContainer>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerAction {
    Start,
    Stop,
    Restart,
    Pause,
    Unpause,
    Kill,
    Remove,
}

impl DockerAction {
    pub fn verb(&self) -> &'static str {
        match self {
            DockerAction::Start => "Start",
            DockerAction::Stop => "Stop",
            DockerAction::Restart => "Restart",
            DockerAction::Pause => "Pause",
            DockerAction::Unpause => "Unpause",
            DockerAction::Kill => "Kill",
            DockerAction::Remove => "Remove",
        }
    }
}

/// Try to connect to the local docker daemon. Returns None if unavailable.
pub fn connect() -> Option<Docker> {
    Docker::connect_with_local_defaults().ok()
}

/// Build a snapshot of all containers, computing CPU% from our own prev-tick deltas.
/// `prev_cpu` maps container id -> (container_total_usage, system_cpu_usage).
pub async fn collect(
    docker: &Docker,
    prev_cpu: &mut HashMap<String, (u64, u64)>,
) -> DockerSnapshot {
    let options = Some(ListContainersOptions::<String> {
        all: true,
        ..Default::default()
    });

    let list = match docker.list_containers(options).await {
        Ok(l) => l,
        Err(e) => {
            return DockerSnapshot {
                available: false,
                error: Some(e.to_string()),
                containers: Vec::new(),
            };
        }
    };

    let mut containers = Vec::new();
    let mut live_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for c in list {
        let id = c.id.clone().unwrap_or_default();
        let name = c
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|s| s.trim_start_matches('/').to_string())
            .unwrap_or_else(|| id.chars().take(12).collect());
        let image = c.image.clone().unwrap_or_default();
        let state = c.state.clone().unwrap_or_default();
        let status = c.status.clone().unwrap_or_default();

        let mut cpu_usage = 0.0;
        let mut mem_mb = 0.0;
        let mut mem_limit_mb = 0.0;

        if state == "running" {
            if let Some(stats) = fetch_stats(docker, &id).await {
                let cur_total = stats.cpu_stats.cpu_usage.total_usage;
                let cur_system = stats.cpu_stats.system_cpu_usage.unwrap_or(0);
                let ncpu = stats
                    .cpu_stats
                    .online_cpus
                    .or_else(|| {
                        stats
                            .cpu_stats
                            .cpu_usage
                            .percpu_usage
                            .as_ref()
                            .map(|v| v.len() as u64)
                    })
                    .unwrap_or(1)
                    .max(1);

                if let Some(&(p_total, p_system)) = prev_cpu.get(&id) {
                    let cpu_delta = cur_total.saturating_sub(p_total) as f64;
                    let sys_delta = cur_system.saturating_sub(p_system) as f64;
                    if sys_delta > 0.0 {
                        cpu_usage = (cpu_delta / sys_delta) * ncpu as f64 * 100.0;
                        if cpu_usage < 0.0 {
                            cpu_usage = 0.0;
                        }
                    }
                }
                prev_cpu.insert(id.clone(), (cur_total, cur_system));
                live_ids.insert(id.clone());

                let usage = stats.memory_stats.usage.unwrap_or(0);
                // Match `docker stats`: subtract page cache from usage when present.
                // cgroup v1 exposes `cache`; cgroup v2 exposes `inactive_file`.
                let cache = match stats.memory_stats.stats {
                    Some(bollard::container::MemoryStatsStats::V1(s)) => s.total_inactive_file,
                    Some(bollard::container::MemoryStatsStats::V2(s)) => s.inactive_file,
                    None => 0,
                };
                let used = usage.saturating_sub(cache);
                mem_mb = used as f64 / 1024.0 / 1024.0;
                mem_limit_mb = stats.memory_stats.limit.unwrap_or(0) as f64 / 1024.0 / 1024.0;
            }
        }

        containers.push(DockerContainer {
            id,
            name,
            image,
            state,
            status,
            cpu_usage,
            mem_mb,
            mem_limit_mb,
        });
    }

    // Drop prev-tick entries for containers that are no longer running.
    prev_cpu.retain(|k, _| live_ids.contains(k));

    // Ordering is decided by the UI layer (AppState sorting), so the list does
    // not reshuffle on every tick. Leave docker's natural (creation) order here.

    DockerSnapshot {
        available: true,
        error: None,
        containers,
    }
}

async fn fetch_stats(docker: &Docker, id: &str) -> Option<bollard::container::Stats> {
    let mut stream = docker.stats(
        id,
        Some(StatsOptions {
            stream: false,
            one_shot: true,
        }),
    );
    match stream.next().await {
        Some(Ok(s)) => Some(s),
        _ => None,
    }
}

/// Run a lifecycle action against a container, targeting the stable `id`.
/// `label` is the friendly name used only for the result/error message.
/// Returns a human-readable result on success or a descriptive error on failure.
pub async fn run_action(
    docker: &Docker,
    action: DockerAction,
    id: &str,
    label: &str,
) -> Result<String, String> {
    let res = match action {
        DockerAction::Start => docker
            .start_container(id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| e.to_string()),
        DockerAction::Stop => docker
            .stop_container(id, None::<StopContainerOptions>)
            .await
            .map_err(|e| e.to_string()),
        DockerAction::Restart => docker
            .restart_container(id, None::<RestartContainerOptions>)
            .await
            .map_err(|e| e.to_string()),
        DockerAction::Pause => docker.pause_container(id).await.map_err(|e| e.to_string()),
        DockerAction::Unpause => docker
            .unpause_container(id)
            .await
            .map_err(|e| e.to_string()),
        DockerAction::Kill => docker
            .kill_container(id, None::<KillContainerOptions<String>>)
            .await
            .map_err(|e| e.to_string()),
        DockerAction::Remove => docker
            .remove_container(
                id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(|e| e.to_string()),
    };

    match res {
        Ok(_) => Ok(format!("{} {}", action.verb(), label)),
        Err(e) => Err(format!("{} {} failed: {}", action.verb(), label, e)),
    }
}

/// Fetch the last `tail` lines of a container's combined stdout/stderr logs.
/// `id` may be a container id or name (the Docker API accepts either).
pub async fn fetch_logs(docker: &Docker, id: &str, tail: usize) -> Vec<String> {
    let opts = LogsOptions::<String> {
        stdout: true,
        stderr: true,
        tail: tail.to_string(),
        timestamps: false,
        ..Default::default()
    };
    let mut stream = docker.logs(id, Some(opts));
    let mut lines = Vec::new();
    while let Some(item) = stream.next().await {
        if let Ok(out) = item {
            let bytes = out.into_bytes();
            let text = String::from_utf8_lossy(&bytes);
            for line in text.lines() {
                lines.push(line.to_string());
            }
        }
    }
    lines
}
