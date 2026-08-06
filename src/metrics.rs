use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::collections::{HashMap, HashSet};

const PROC_PATH: &str = "/proc";
const PROC_STAT_PATH: &str = "/proc/stat";
const PROC_MEMINFO_PATH: &str = "/proc/meminfo";
const PROC_NET_DEV_PATH: &str = "/proc/net/dev";
const PROC_CPU_INFO: &str ="/proc/cpuinfo";

#[derive(Debug, Clone)]
pub struct CpuRawState {
    pub total: u64,
    pub idle: u64,
    /// (total, idle) per logical CPU, indexed by cpuN from /proc/stat.
    pub cores: Vec<(u64, u64)>,
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryMetrics {
    pub total_mem_kb: u64,
    pub used_mem_kb: u64,
    pub total_swap_kb: u64,
    pub used_swap_kb: u64,
}

#[derive(Debug, Clone)]
pub struct ProcessMetrics {
    pub pid: u32,
    pub name: String,
    pub user: String,
    pub cpu_usage: f64,
    pub mem_res_kb: u64,
    /// Raw `Cpus_allowed_list` from /proc/<pid>/status, e.g. "0-3,5".
    pub affinity: String,
}

#[derive(Debug, Clone, Copy)]
pub struct NetworkMetrics {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct SystemSnapshot {
    pub cpu_usage: f64,
    pub core_usage: Vec<f64>,
    pub numa_nodes: Vec<NumaNode>,
    pub hugepages: HugePageInfo,
    pub interfaces: Vec<InterfaceSnapshot>,
    pub tcp_retrans: u64,
    pub clock: ClockSync,
    pub multicast: MulticastSummary,
    pub mem: MemoryMetrics,
    pub net_rx_speed: f64,
    pub net_tx_speed: f64,
    pub core_info: CoreInfo,
    pub processes: Vec<ProcessMetrics>,
    pub load_avg: LoadAverage,
    pub disk_io: DiskIOSpeed,
    pub disk_partitions: Vec<DiskPartition>,
}

#[derive(Debug, Clone)]
pub struct CoreInfo {
    pub cpu_model: String,
    pub vendor: String,
    pub physical_core: u64,
}

/// A single NUMA node: its memory totals and the logical CPUs local to it.
#[derive(Debug, Clone)]
pub struct NumaNode {
    pub id: u32,
    pub mem_total_kb: u64,
    pub mem_used_kb: u64,
    /// Logical CPU ids that belong to this node (ascending).
    pub cpus: Vec<u32>,
}

/// Compress a sorted list of cpu ids into a compact range string, e.g.
/// `[0,1,2,3,5] -> "0-3,5"`.
pub fn format_cpu_list(cpus: &[u32]) -> String {
    let mut out = String::new();
    let mut it = cpus.iter().copied();
    let Some(mut start) = it.next() else { return "none".to_string() };
    let mut prev = start;

    let flush = |start: u32, prev: u32, out: &mut String| {
        if !out.is_empty() {
            out.push(',');
        }
        if start == prev {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{}-{}", start, prev));
        }
    };

    for c in it {
        if c == prev + 1 {
            prev = c;
        } else {
            flush(start, prev, &mut out);
            start = c;
            prev = c;
        }
    }
    flush(start, prev, &mut out);
    out
}

pub fn fetch_numa_nodes() -> Vec<NumaNode> {
    const NODE_DIR: &str = "/sys/devices/system/node";
    let mut nodes = Vec::new();
    let Ok(entries) = fs::read_dir(NODE_DIR) else {
        return nodes;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(id_str) = name.strip_prefix("node") else { continue };
        let Ok(id) = id_str.parse::<u32>() else { continue };
        let base = format!("{}/{}", NODE_DIR, name);

        let mut total = 0u64;
        let mut free = 0u64;
        if let Ok(content) = fs::read_to_string(format!("{}/meminfo", base)) {
            for line in content.lines() {
                if let Some((key, value)) = line.split_once(':') {
                    let v = value
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    let k = key.trim();
                    if k.ends_with("MemTotal") {
                        total = v;
                    } else if k.ends_with("MemFree") {
                        free = v;
                    }
                }
            }
        }

        let mut cpus = Vec::new();
        if let Ok(cpu_entries) = fs::read_dir(&base) {
            for c in cpu_entries.flatten() {
                let cn = c.file_name();
                let cn = cn.to_string_lossy();
                if let Some(rest) = cn.strip_prefix("cpu") {
                    if !rest.contains('_') {
                        if let Ok(n) = rest.parse::<u32>() {
                            cpus.push(n);
                        }
                    }
                }
            }
        }
        cpus.sort_unstable();

        nodes.push(NumaNode {
            id,
            mem_total_kb: total,
            mem_used_kb: total.saturating_sub(free),
            cpus,
        });
    }

    nodes.sort_by_key(|n| n.id);
    nodes
}

/// A single hugepage pool for a given page size.
#[derive(Debug, Clone)]
pub struct HugePagePool {
    pub size_kb: u64,
    pub nr: u64,
    pub free: u64,
    pub resv: u64,
    pub surplus: u64,
}

/// Hugepage pools plus the transparent-hugepage mode.
#[derive(Debug, Clone, Default)]
pub struct HugePageInfo {
    pub pools: Vec<HugePagePool>,
    pub thp_enabled: String, // "always" | "madvise" | "never" ("" if unavailable)
}

/// Extract the currently-active mode from a THP `enabled` file, which looks
/// like `always [madvise] never` (the chosen mode is wrapped in brackets).
fn extract_thp_mode(s: &str) -> String {
    s.split_whitespace()
        .find(|w| w.starts_with('[') && w.ends_with(']'))
        .map(|w| w.trim_matches(&['[', ']'][..]).to_string())
        .unwrap_or_default()
}

impl HugePageInfo {
    pub fn fetch_all() -> Self {
        const DIR: &str = "/sys/kernel/mm/hugepages";
        let mut pools = Vec::new();

        if let Ok(entries) = fs::read_dir(DIR) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Some(sz) = name.strip_prefix("hugepages-").and_then(|s| s.strip_suffix("kB"))
                else {
                    continue;
                };
                let Ok(size_kb) = sz.parse::<u64>() else { continue };
                let base = format!("{}/{}", DIR, name);
                let rd = |f: &str| -> u64 {
                    fs::read_to_string(format!("{}/{}", base, f))
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0)
                };
                pools.push(HugePagePool {
                    size_kb,
                    nr: rd("nr_hugepages"),
                    free: rd("free_hugepages"),
                    resv: rd("resv_hugepages"),
                    surplus: rd("surplus_hugepages"),
                });
            }
        }
        pools.sort_by_key(|p| p.size_kb);

        let thp_enabled = fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
            .map(|s| extract_thp_mode(&s))
            .unwrap_or_default();

        HugePageInfo { pools, thp_enabled }
    }
}

impl CpuRawState {
    pub fn read_from_system() -> std::io::Result<Self> {
        let file = File::open(PROC_STAT_PATH)?;
        let reader = BufReader::new(file);

        let mut total = 0u64;
        let mut idle = 0u64;
        let mut cores: Vec<(u64, u64)> = Vec::new();

        for line in reader.lines() {
            let line = line?;
            let mut parts = line.split_whitespace();
            let Some(prefix) = parts.next() else { continue };
            if prefix == "cpu" {
                let metrics: Vec<u64> = parts.map(|s| s.parse::<u64>().unwrap_or(0)).collect();
                if metrics.len() < 4 {
                    continue;
                }
                total = metrics.iter().sum();
                idle = metrics[3] + metrics[4]; // idle + iowait
            } else if prefix.starts_with("cpu") {
                // Per-core line, e.g. cpu0, cpu1, ... in ascending order.
                let metrics: Vec<u64> = parts.map(|s| s.parse::<u64>().unwrap_or(0)).collect();
                if metrics.len() >= 4 {
                    cores.push((metrics.iter().sum(), metrics[3] + metrics[4]));
                }
            } else {
                break; // aggregate + per-core lines come first, then non-cpu rows
            }
        }

        Ok(CpuRawState { total, idle, cores })
    }

    pub fn calculate_usage (prev: &Self, current: &Self) -> f64 {
        let total_diff = current.total.saturating_sub(prev.total) as f64;
        let idle_diff = current.idle.saturating_sub(prev.idle) as f64;
        if total_diff == 0.0 {
            return 0.0;
        }
        let usage = (total_diff - idle_diff) / total_diff * 100.0;
        usage.clamp(0.0, 100.0)
    }

    /// Per-logical-CPU utilization (0-100%) across consecutive reads.
    pub fn calculate_usage_per_core(prev: &Self, current: &Self) -> Vec<f64> {
        let mut usage = Vec::with_capacity(current.cores.len());
        for (i, &(cur_total, cur_idle)) in current.cores.iter().enumerate() {
            let (prev_total, prev_idle) = prev.cores.get(i).copied().unwrap_or((cur_total, cur_idle));
            let total_diff = cur_total.saturating_sub(prev_total) as f64;
            let idle_diff = cur_idle.saturating_sub(prev_idle) as f64;
            if total_diff <= 0.0 {
                usage.push(0.0);
            } else {
                usage.push(((total_diff - idle_diff) / total_diff * 100.0).clamp(0.0, 100.0));
            }
        }
        usage
    }
}

impl MemoryMetrics {
    pub fn read_from_system() -> std::io::Result<Self> {
        let file = File::open(PROC_MEMINFO_PATH)?;
        let reader = BufReader::new(file);

        let mut mem_total = 0;
        let mut mem_available = 0;
        let mut swap_total = 0;
        let mut swap_free = 0;

        for line in reader.lines() {
            let line = line?;
            let mut parts = line.split_whitespace();
            if let Some(key) = parts.next() {
                if let Some(value_str) = parts.next(){
                    let value = value_str.parse::<u64>().unwrap_or(0);
                    match key {
                        "MemTotal:" => mem_total = value,
                        "MemAvailable:" => mem_available = value, 
                        "SwapTotal:" => swap_total = value,
                        "SwapFree:" => swap_free = value,
                        _ => {}
                    }
                }
            }
        }
        let used_mem = mem_total.saturating_sub(mem_available);
        let used_swap = swap_total.saturating_sub(swap_free);

        Ok(MemoryMetrics {
            total_mem_kb: mem_total,
            used_mem_kb: used_mem,
            total_swap_kb: swap_total,
            used_swap_kb: used_swap,
        })
    }
}

fn parse_passwd() -> HashMap<u32, String> {
    let mut map = HashMap::new();
    if let Ok(content) = fs::read_to_string("/etc/passwd") {
        for line in content.lines() {
            let mut parts = line.split(':');
            if let (Some(name), Some(_), Some(uid_str)) = (parts.next(), parts.next(), parts.next()) {
                if let Ok(uid) = uid_str.parse::<u32>() {
                    map.insert(uid, name.to_string());
                }
            }
        }
    }
    map
}

/// Parse the contents of `/proc/<pid>/stat` into `(comm, cpu_ticks)` where
/// `cpu_ticks = utime + stime`. Pure and defensive: handles truncated input
/// and process names that themselves contain parentheses (the `comm` field is
/// delimited by the first `(` and the last `)`). Returns sensible defaults
/// ("Unknown", 0) when the expected shape is absent.
fn parse_stat(stat_content: &str) -> (String, u64) {
    let mut name = String::from("Unknown");
    let mut cpu_ticks = 0u64;

    if let Some(open_paren) = stat_content.find('(') {
        if let Some(close_paren) = stat_content.rfind(')') {
            // Defensive slicing: a process can exit mid-read and truncate the file.
            if close_paren > open_paren {
                if let Some(n) = stat_content.get(open_paren + 1..close_paren) {
                    name = n.to_string();
                }
            }
            if let Some(tail) = stat_content.get(close_paren + 2..) {
                let tail_parts: Vec<&str> = tail.split_whitespace().collect();
                // tail field 11 = utime, field 12 = stime (0-indexed after state)
                let utime = tail_parts.get(11).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                let stime = tail_parts.get(12).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                cpu_ticks = utime + stime;
            }
        }
    }

    (name, cpu_ticks)
}

impl ProcessMetrics{
    pub fn fetch_all(prev_ticks: &mut HashMap<u32, u64>, total_ticks_delta: u64) -> std::io::Result<Vec<Self>>{
        let uid_map = parse_passwd();
        let mut processes = Vec::new();
        for entry in fs::read_dir(PROC_PATH)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir(){
                if let Some(pid_str) = path.file_name().and_then(|s| s.to_str()){
                    if let Ok(pid) = pid_str.parse::<u32>(){
                        if let Ok((mut proc_info, cpu_ticks)) = Self::read_process_info(pid, &uid_map){
                            if total_ticks_delta > 0 {
                                let prev = prev_ticks.get(&pid).copied().unwrap_or(cpu_ticks);
                                let delta = cpu_ticks.saturating_sub(prev);
                                proc_info.cpu_usage = (delta as f64 / total_ticks_delta as f64 * 100.0).clamp(0.0, 100.0);
                            }
                            prev_ticks.insert(pid, cpu_ticks);
                            processes.push(proc_info);
                        }
                    }
                }
            }
        }

        processes.sort_by_key(|p| std::cmp::Reverse(p.mem_res_kb));
        Ok(processes)
    }

    fn read_process_info(pid: u32, uid_map: &HashMap<u32, String>) -> std::io::Result<(Self, u64)>{
        let stat_path = format!("{}/{}/stat", PROC_PATH, pid);
        let stat_content = fs::read_to_string(stat_path)?;
        let (name, cpu_ticks) = parse_stat(&stat_content);

        // Read UID and CPU affinity from /proc/<pid>/status
        let status_path = format!("{}/{}/status", PROC_PATH, pid);
        let mut user = String::from("?");
        let mut affinity = String::from("all");
        if let Ok(status) = fs::read_to_string(status_path) {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("Uid:") {
                    if let Some(uid_str) = rest.split_whitespace().next() {
                        if let Ok(uid) = uid_str.parse::<u32>() {
                            user = uid_map.get(&uid)
                                .cloned()
                                .unwrap_or_else(|| uid.to_string());
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("Cpus_allowed_list:") {
                    let v = rest.trim();
                    if !v.is_empty() {
                        affinity = v.to_string();
                    }
                }
            }
        }

        let statm_path = format!("{}/{}/statm", PROC_PATH, pid);
        let statm_content = fs::read_to_string(statm_path)?;
        let mut parts = statm_content.split_whitespace();
        parts.next();
        let res_pages = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        // Use the real page size rather than assuming 4 KiB.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page_kb = if page_size > 0 { page_size as u64 / 1024 } else { 4 };
        let mem_res_kb = res_pages * page_kb;

        Ok((ProcessMetrics { pid, name, user, cpu_usage: 0.0, mem_res_kb, affinity }, cpu_ticks))
    }

    /// Pin `pid` to a single logical CPU via `sched_setaffinity`.
    /// Only the process owner can do this without privileges; other users
    /// require root (the caller surfaces the error to the UI).
    pub fn set_affinity(pid: u32, core: u32) -> std::io::Result<()> {
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::CPU_ZERO(&mut set);
            libc::CPU_SET(core as usize, &mut set);
        }
        let res = unsafe {
            libc::sched_setaffinity(pid as libc::pid_t, std::mem::size_of::<libc::cpu_set_t>(), &set)
        };
        if res != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl NetworkMetrics {
    pub fn read_from_system() -> std::io::Result<Self> {
        let file = File::open(PROC_NET_DEV_PATH)?;
        let reader = BufReader::new(file);
        let mut rx_bytes = 0;
        let mut tx_bytes = 0;

        for line in reader.lines().skip(2){
            let line = line?;
            let mut parts = line.split_whitespace();
            if let Some(interface_part) = parts.next(){
                if interface_part != "lo:"{
                    if let Some(rx_str) = parts.next(){
                        rx_bytes += rx_str.parse::<u64>().unwrap_or(0);
                    }
                    for _ in 0..7 { parts.next(); }
                    if let Some(tx_str) = parts.next() {
                        tx_bytes += tx_str.parse::<u64>().unwrap_or(0);
                    }
                }
            }
        }
        Ok(NetworkMetrics { rx_bytes, tx_bytes })
    }

    pub fn calculate_speed(prev: &Self, current: &Self, duration_secs: f64) -> (f64, f64) {
        if duration_secs <= 0.0 { return (0.0, 0.0); }
        
        let rx_delta = current.rx_bytes.saturating_sub(prev.rx_bytes);
        let tx_delta = current.tx_bytes.saturating_sub(prev.tx_bytes);

        let rx_speed_kbs = (rx_delta as f64 / 1024.0) / duration_secs;
        let tx_speed_kbs = (tx_delta as f64 / 1024.0) / duration_secs;

        (rx_speed_kbs, tx_speed_kbs)
    }
}

/// Cumulative per-interface counters read from `/proc/net/dev` (interface name
/// with the trailing `:` stripped). `lo` is excluded.
#[derive(Debug, Clone)]
pub struct InterfaceMetrics {
    pub name: String,
    pub rx_bytes: u64,
    pub rx_errs: u64,
    pub rx_drops: u64,
    pub multicast: u64,
    pub tx_bytes: u64,
    pub tx_errs: u64,
    pub tx_drops: u64,
}

impl InterfaceMetrics {
    pub fn read_all() -> Vec<Self> {
        let mut out = Vec::new();
        if let Ok(content) = fs::read_to_string("/proc/net/dev") {
            for line in content.lines().skip(2) {
                let mut parts = line.split_whitespace();
                let Some(face) = parts.next() else { continue };
                let name = face.trim_end_matches(':').to_string();
                if name == "lo" {
                    continue;
                }
                let vals: Vec<u64> = parts.map(|s| s.parse::<u64>().unwrap_or(0)).collect();
                if vals.len() < 16 {
                    continue;
                }
                // receive: bytes packets errs drop fifo frame compressed multicast
                // transmit: bytes packets errs drop fifo colls carrier compressed
                out.push(InterfaceMetrics {
                    name,
                    rx_bytes: vals[0],
                    rx_errs: vals[2],
                    rx_drops: vals[3],
                    multicast: vals[7],
                    tx_bytes: vals[8],
                    tx_errs: vals[10],
                    tx_drops: vals[11],
                });
            }
        }
        out
    }
}

/// Per-interface snapshot with RX/TX rates computed between ticks.
#[derive(Debug, Clone)]
pub struct InterfaceSnapshot {
    pub name: String,
    pub rx_kbs: f64,
    pub tx_kbs: f64,
    pub rx_errs: u64,
    pub rx_drops: u64,
    pub tx_errs: u64,
    pub tx_drops: u64,
    pub multicast: u64,
}

fn delta_rate(prev: Option<u64>, cur: u64, dur: f64) -> f64 {
    if dur <= 0.0 {
        return 0.0;
    }
    let d = cur.saturating_sub(prev.unwrap_or(cur)) as f64;
    (d / 1024.0) / dur
}

/// Diff two cumulative per-interface reads into a snapshot list with rates.
pub fn compute_interface_speeds(
    prev: &[InterfaceMetrics],
    cur: &[InterfaceMetrics],
    duration_secs: f64,
) -> Vec<InterfaceSnapshot> {
    cur.iter()
        .map(|c| {
            let p = prev.iter().find(|p| p.name == c.name);
            InterfaceSnapshot {
                name: c.name.clone(),
                rx_kbs: delta_rate(p.map(|p| p.rx_bytes), c.rx_bytes, duration_secs),
                tx_kbs: delta_rate(p.map(|p| p.tx_bytes), c.tx_bytes, duration_secs),
                rx_errs: c.rx_errs,
                rx_drops: c.rx_drops,
                tx_errs: c.tx_errs,
                tx_drops: c.tx_drops,
                multicast: c.multicast,
            }
        })
        .collect()
}

/// Number of TCP retransmitted segments from `/proc/net/snmp`.
pub fn tcp_retransmits() -> u64 {
    if let Ok(content) = fs::read_to_string("/proc/net/snmp") {
        let mut lines = content.lines();
        while let Some(l) = lines.next() {
            if !l.starts_with("Tcp:") {
                continue;
            }
            let names: Vec<&str> = l.split_whitespace().collect();
            if let Some(next) = lines.next() {
                if next.starts_with("Tcp:") {
                    let vals: Vec<&str> = next.split_whitespace().collect();
                    if let Some(idx) = names.iter().position(|n| *n == "RetransSegs") {
                        return vals.get(idx).and_then(|s| s.parse().ok()).unwrap_or(0);
                    }
                }
            }
            break;
        }
    }
    0
}

/// Clock sync status: NTP/PLL offset from the `adjtimex(2)` syscall plus the
/// first available PTP hardware clock name.
#[derive(Debug, Clone)]
pub struct ClockSync {
    /// NTP/PLL time offset in microseconds (negative = local clock behind).
    pub offset_us: i64,
    /// Human-readable `adjtimex` time-state (e.g. "TIME_OK", "TIME_ERROR").
    pub state: String,
    /// PTP clock device name (e.g. "hyperv"), or "n/a".
    pub ptp_clock: String,
}

fn adjtimex_state_str(code: i32) -> &'static str {
    match code {
        0 => "TIME_OK",
        1 => "TIME_INS",
        2 => "TIME_DEL",
        3 => "TIME_OOP",
        4 => "TIME_WAIT",
        _ => "TIME_ERROR",
    }
}

pub fn read_clock_sync() -> ClockSync {
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    let state_code = unsafe { libc::adjtimex(&mut tx) };
    let offset_us = tx.offset;

    let ptp_clock = fs::read_to_string("/sys/class/ptp/ptp0/clock_name")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "n/a".to_string());

    ClockSync {
        offset_us,
        state: adjtimex_state_str(state_code).to_string(),
        ptp_clock,
    }
}

/// Multicast (IGMP / MLD) group membership summary.
///
/// IPv4 memberships come from `/proc/net/igmp` (group addresses are stored as
/// little-endian hex); IPv6 from `/proc/net/igmp6`. Both count group
/// memberships per interface — a stream is received on an interface only if it
/// is in a group there, so this is a useful read-only check for multicast
/// trading feeds.
#[derive(Debug, Clone)]
pub struct MulticastSummary {
    pub ipv4_memberships: usize,
    pub ipv4_interfaces: usize,
    pub ipv6_memberships: usize,
    pub ipv6_interfaces: usize,
}

fn count_igmp(content: &str) -> Vec<(String, usize)> {
    let mut devices: Vec<(String, usize)> = Vec::new();
    let mut current: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A device line is "1\teth3      : 2 V3": the prefix before the first
        // ':' is "<index> <device>" where <index> is numeric. The header line
        // ("Idx Device : ...") and group lines (which embed a ':' inside their
        // timer field) do not start with a numeric index.
        if let Some(pos) = trimmed.find(':') {
            let fields: Vec<&str> = trimmed[..pos].split_whitespace().collect();
            if fields.len() >= 2 && fields[0].chars().all(|c| c.is_ascii_digit()) {
                current = Some(fields[1].to_string());
                devices.push((fields[1].to_string(), 0));
                continue;
            }
        }
        // Not a device line: treat as a group line of the current device.
        if let Some(cur) = &current {
            if let Some((_, count)) = devices.iter_mut().find(|(n, _)| n == cur) {
                *count += 1;
            }
        }
    }
    devices
}

fn parse_igmp() -> Vec<(String, usize)> {
    fs::read_to_string("/proc/net/igmp")
        .map(|content| count_igmp(&content))
        .unwrap_or_default()
}

fn count_igmp6(content: &str) -> Vec<(String, usize)> {
    let mut devices: Vec<(String, usize)> = Vec::new();
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let _idx = parts.next();
        if let Some(dev) = parts.next() {
            if let Some((_, count)) = devices.iter_mut().find(|(n, _)| n == dev) {
                *count += 1;
            } else {
                devices.push((dev.to_string(), 1));
            }
        }
    }
    devices
}

fn parse_igmp6() -> Vec<(String, usize)> {
    fs::read_to_string("/proc/net/igmp6")
        .map(|content| count_igmp6(&content))
        .unwrap_or_default()
}

pub fn read_multicast() -> MulticastSummary {
    let v4 = parse_igmp();
    let v6 = parse_igmp6();
    MulticastSummary {
        ipv4_memberships: v4.iter().map(|(_, c)| *c).sum(),
        ipv4_interfaces: v4.len(),
        ipv6_memberships: v6.iter().map(|(_, c)| *c).sum(),
        ipv6_interfaces: v6.len(),
    }
}

impl CoreInfo {
    pub fn read_from_system() -> std::io::Result<Self> {
        let file = File::open(PROC_CPU_INFO)?;
        let reader = BufReader::new(file);

        let mut cpu_model = String::from("Unknown");
        let mut vendor = String::from("Unknown");
        let mut physical_core = 0;

        for line in reader.lines() {
            let line = line?;
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim();
                let value = value.trim();
                match key {
                    "model name" => cpu_model = value.to_string(),
                    "vendor_id" => vendor = value.to_string(),
                    "cpu cores" => physical_core = value.parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        Ok(CoreInfo { cpu_model, vendor, physical_core })
    }
}

#[derive(Debug, Clone)]
pub struct LoadAverage {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

impl LoadAverage {
    pub fn read_from_system() -> std::io::Result<Self> {
        let content = fs::read_to_string("/proc/loadavg")?;
        let mut parts = content.split_whitespace();
        let one = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let five = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let fifteen = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        Ok(LoadAverage { one, five, fifteen })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DiskIOState {
    pub sectors_read: u64,
    pub sectors_written: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct DiskIOSpeed {
    pub read_kbs: f64,
    pub write_kbs: f64,
}

impl DiskIOState {
    pub fn read_from_system() -> std::io::Result<Self> {
        let file = File::open("/proc/diskstats")?;
        let reader = BufReader::new(file);
        let mut sectors_read = 0u64;
        let mut sectors_written = 0u64;
        for line in reader.lines() {
            let line = line?;
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 10 { continue; }
            let dev = parts[2];
            // Only whole disks (present in /sys/block/), not partitions
            if !Path::new(&format!("/sys/block/{}", dev)).exists() { continue; }
            sectors_read += parts[5].parse::<u64>().unwrap_or(0);
            sectors_written += parts[9].parse::<u64>().unwrap_or(0);
        }
        Ok(DiskIOState { sectors_read, sectors_written })
    }

    pub fn calculate_speed(prev: &Self, current: &Self, duration_secs: f64) -> DiskIOSpeed {
        if duration_secs <= 0.0 { return DiskIOSpeed { read_kbs: 0.0, write_kbs: 0.0 }; }
        let read_delta = current.sectors_read.saturating_sub(prev.sectors_read);
        let write_delta = current.sectors_written.saturating_sub(prev.sectors_written);
        // Each sector = 512 bytes
        DiskIOSpeed {
            read_kbs: (read_delta as f64 * 512.0 / 1024.0) / duration_secs,
            write_kbs: (write_delta as f64 * 512.0 / 1024.0) / duration_secs,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiskPartition {
    pub total_gb: f64,
    pub used_gb: f64,
    pub use_pct: f64,
}

impl DiskPartition {
    pub fn fetch_all() -> Vec<Self> {
        let mut partitions = Vec::new();
        let content = match fs::read_to_string("/proc/mounts") {
            Ok(c) => c,
            Err(_) => return partitions,
        };
        let mut seen: HashSet<String> = HashSet::new();
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 { continue; }
            let device = parts[0];
            let mount = parts[1];
            let fstype = parts[2];
            match fstype {
                "tmpfs" | "devtmpfs" | "sysfs" | "proc" | "cgroup" | "cgroup2" |
                "devpts" | "securityfs" | "pstore" | "efivarfs" | "bpf" |
                "autofs" | "hugetlbfs" | "mqueue" | "debugfs" | "tracefs" |
                "fusectl" | "configfs" | "ramfs" | "overlay" | "squashfs" => continue,
                _ => {}
            }
            if !device.starts_with('/') { continue; }
            if !seen.insert(device.to_string()) { continue; }
            let mount_cstr = match std::ffi::CString::new(mount) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statvfs(mount_cstr.as_ptr(), &mut stat) } != 0 { continue; }
            let block_size = stat.f_frsize as u64;
            let total = stat.f_blocks * block_size;
            let available = stat.f_bavail * block_size;
            let used = total.saturating_sub(available);
            if total == 0 { continue; }
            partitions.push(DiskPartition {
                total_gb: total as f64 / 1024.0 / 1024.0 / 1024.0,
                used_gb: used as f64 / 1024.0 / 1024.0 / 1024.0,
                use_pct: used as f64 / total as f64 * 100.0,
            });
        }
        partitions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_usage_basic() {
        let prev = CpuRawState { total: 100, idle: 50, cores: Vec::new() };
        let cur = CpuRawState { total: 200, idle: 100, cores: Vec::new() };
        // busy = (100 - 50) / 100 = 50%
        assert!((CpuRawState::calculate_usage(&prev, &cur) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn cpu_usage_no_delta_is_zero() {
        let s = CpuRawState { total: 100, idle: 50, cores: Vec::new() };
        assert_eq!(CpuRawState::calculate_usage(&s, &s), 0.0);
    }

    #[test]
    fn cpu_usage_is_clamped() {
        // idle decreasing more than total still clamps into [0, 100]
        let prev = CpuRawState { total: 100, idle: 100, cores: Vec::new() };
        let cur = CpuRawState { total: 110, idle: 100, cores: Vec::new() };
        let u = CpuRawState::calculate_usage(&prev, &cur);
        assert!((0.0..=100.0).contains(&u));
    }

    #[test]
    fn per_core_usage_basic() {
        let prev = CpuRawState {
            total: 300, idle: 150,
            cores: vec![(100, 50), (100, 90), (100, 25)],
        };
        let cur = CpuRawState {
            total: 600, idle: 300,
            cores: vec![(200, 100), (200, 180), (200, 50)],
        };
        let u = CpuRawState::calculate_usage_per_core(&prev, &cur);
        // each core busy = (total-idle)/total
        assert!((u[0] - 50.0).abs() < 1e-9);
        assert!((u[1] - 10.0).abs() < 1e-9);
        assert!((u[2] - 75.0).abs() < 1e-9);
    }

    #[test]
    fn format_cpu_list_compresses_ranges() {
        assert_eq!(format_cpu_list(&[]), "none");
        assert_eq!(format_cpu_list(&[0]), "0");
        assert_eq!(format_cpu_list(&[0, 1, 2, 3]), "0-3");
        assert_eq!(format_cpu_list(&[0, 1, 2, 3, 5]), "0-3,5");
        assert_eq!(format_cpu_list(&[0, 2, 4, 6]), "0,2,4,6");
        assert_eq!(format_cpu_list(&[4, 5, 6, 10, 11, 12]), "4-6,10-12");
    }

    #[test]
    fn thp_mode_extraction() {
        assert_eq!(extract_thp_mode("always [madvise] never"), "madvise");
        assert_eq!(extract_thp_mode("[always] never"), "always");
        assert_eq!(extract_thp_mode("always never"), "");
        assert_eq!(extract_thp_mode(""), "");
    }

    #[test]
    fn net_speed_kbs() {
        let prev = NetworkMetrics { rx_bytes: 0, tx_bytes: 0 };
        let cur = NetworkMetrics { rx_bytes: 2048, tx_bytes: 1024 };
        let (rx, tx) = NetworkMetrics::calculate_speed(&prev, &cur, 2.0);
        assert!((rx - 1.0).abs() < 1e-9); // 2048 B over 2 s = 1 KB/s
        assert!((tx - 0.5).abs() < 1e-9);
    }

    #[test]
    fn net_speed_zero_duration() {
        let prev = NetworkMetrics { rx_bytes: 0, tx_bytes: 0 };
        let cur = NetworkMetrics { rx_bytes: 5000, tx_bytes: 5000 };
        assert_eq!(NetworkMetrics::calculate_speed(&prev, &cur, 0.0), (0.0, 0.0));
    }

    #[test]
    fn interface_speeds_diff() {
        let prev = vec![InterfaceMetrics {
            name: "eth0".into(), rx_bytes: 0, rx_errs: 0, rx_drops: 0, multicast: 0,
            tx_bytes: 0, tx_errs: 0, tx_drops: 0,
        }];
        let cur = vec![InterfaceMetrics {
            name: "eth0".into(), rx_bytes: 2048, rx_errs: 1, rx_drops: 2, multicast: 3,
            tx_bytes: 1024, tx_errs: 4, tx_drops: 5,
        }];
        let snaps = compute_interface_speeds(&prev, &cur, 2.0);
        assert_eq!(snaps.len(), 1);
        let s = &snaps[0];
        assert!((s.rx_kbs - 1.0).abs() < 1e-9); // 2048 B over 2 s
        assert!((s.tx_kbs - 0.5).abs() < 1e-9);
        assert_eq!(s.rx_errs, 1);
        assert_eq!(s.multicast, 3);
    }

    #[test]
    fn interface_spe_zero_duration() {
        let prev = vec![InterfaceMetrics {
            name: "eth0".into(), rx_bytes: 0, rx_errs: 0, rx_drops: 0, multicast: 0,
            tx_bytes: 0, tx_errs: 0, tx_drops: 0,
        }];
        let cur = prev.clone();
        let snaps = compute_interface_speeds(&prev, &cur, 0.0);
        assert_eq!((snaps[0].rx_kbs, snaps[0].tx_kbs), (0.0, 0.0));
    }

    #[test]
    fn interface_rate_handles_wrap() {
        // new interface absent from prev → rate uses cur as baseline (0)
        let cur = vec![InterfaceMetrics {
            name: "eth9".into(), rx_bytes: 4096, rx_errs: 0, rx_drops: 0, multicast: 0,
            tx_bytes: 4096, tx_errs: 0, tx_drops: 0,
        }];
        let snaps = compute_interface_speeds(&[], &cur, 1.0);
        assert_eq!((snaps[0].rx_kbs, snaps[0].tx_kbs), (0.0, 0.0));
    }

    #[test]
    fn adjtimex_state_map() {
        assert_eq!(adjtimex_state_str(0), "TIME_OK");
        assert_eq!(adjtimex_state_str(1), "TIME_INS");
        assert_eq!(adjtimex_state_str(5), "TIME_ERROR");
        assert_eq!(adjtimex_state_str(-1), "TIME_ERROR");
    }

    #[test]
    fn count_igmp_groups_per_iface() {
        let content = "\
Idx	Device    : Count Querier\tGroup    Users Timer	Reporter
1	lo        :     2      V3
				FB0000E0     1 0:00000000		0
				010000E0     1 0:00000000		0
6	eth3      :     2      V3
				FB0000E0     1 0:00000000		0
				010000E0     1 0:00000000		0
";
        let groups = count_igmp(content);
        assert_eq!(groups, vec![("lo".to_string(), 2), ("eth3".to_string(), 2)]);
    }

    #[test]
    fn count_igmp_skips_header_no_orphan_groups() {
        // A group line before any interface line must not crash or count.
        let content = "\
Idx	Device    : 		Group    Users Timer	Reporter
				FB0000E0     1 0:00000000		0
6	eth3      :     2      V3
				010000E0     1 0:00000000		0
";
        let groups = count_igmp(content);
        assert_eq!(groups, vec![("eth3".to_string(), 1)]);
    }

    #[test]
    fn count_igmp6_groups_per_iface() {
        let content = "\
1    lo              ff0200000000000000000000000000fb     1 00000004 0
1    lo              ff010000000000000000000000000001     1 00000008 0
6    eth3            ff010000000000000000000000000001     1 00000008 0
";
        let groups = count_igmp6(content);
        assert_eq!(groups, vec![("lo".to_string(), 2), ("eth3".to_string(), 1)]);
    }

    #[test]
    fn disk_speed_kbs() {
        let prev = DiskIOState { sectors_read: 0, sectors_written: 0 };
        let cur = DiskIOState { sectors_read: 4, sectors_written: 2 };
        // each sector = 512 B → 4 sectors = 2 KB, 2 sectors = 1 KB, over 1 s
        let speed = DiskIOState::calculate_speed(&prev, &cur, 1.0);
        assert!((speed.read_kbs - 2.0).abs() < 1e-9);
        assert!((speed.write_kbs - 1.0).abs() < 1e-9);
    }

    #[test]
    fn disk_speed_zero_duration() {
        let prev = DiskIOState { sectors_read: 0, sectors_written: 0 };
        let cur = DiskIOState { sectors_read: 10, sectors_written: 10 };
        let speed = DiskIOState::calculate_speed(&prev, &cur, 0.0);
        assert_eq!((speed.read_kbs, speed.write_kbs), (0.0, 0.0));
    }

    #[test]
    fn parse_stat_normal() {
        // state(0) ... utime(11)=50 stime(12)=25 -> 75 ticks
        let line = "1234 (bash) S 1 1234 1234 0 -1 4194304 100 0 0 0 50 25 0 0";
        assert_eq!(parse_stat(line), ("bash".to_string(), 75));
    }

    #[test]
    fn parse_stat_name_with_parentheses() {
        // comm is delimited by first '(' and last ')'
        let line = "999 (my (proc)) S 1 2 3 4 5 6 7 8 9 10 11 12 13";
        let (name, _) = parse_stat(line);
        assert_eq!(name, "my (proc)");
    }

    #[test]
    fn parse_stat_truncated_no_close_paren() {
        assert_eq!(parse_stat("123 (trunc"), ("Unknown".to_string(), 0));
    }

    #[test]
    fn parse_stat_empty() {
        assert_eq!(parse_stat(""), ("Unknown".to_string(), 0));
    }

    #[test]
    fn parse_stat_missing_cpu_fields() {
        // name present, but no utime/stime fields available
        assert_eq!(parse_stat("1 (x) S"), ("x".to_string(), 0));
    }
}