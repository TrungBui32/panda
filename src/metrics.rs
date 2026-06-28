use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::collections::{HashMap, HashSet};

const PROC_PATH: &str = "/proc";
const PROC_STAT_PATH: &str = "/proc/stat";
const PROC_MEMINFO_PATH: &str = "/proc/meminfo";
const PROC_NET_DEV_PATH: &str = "/proc/net/dev";
const PROC_CPU_INFO: &str ="/proc/cpuinfo";

#[derive(Debug, Clone, Copy)]
pub struct CpuRawState {
    pub total: u64,
    pub idle: u64,
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
}

#[derive(Debug, Clone, Copy)]
pub struct NetworkMetrics {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct SystemSnapshot {
    pub cpu_usage: f64,
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

impl CpuRawState {
    pub fn read_from_system() -> std::io::Result<Self> {
        let file = File::open(PROC_STAT_PATH)?;
        let mut reader = BufReader::new(file);  
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let mut parts = line.split_whitespace();
        
        // Skip the "cpu" prefix
        parts.next();

        let metrics: Vec<u64> = parts.map(|s| s.parse::<u64>().unwrap_or(0)).collect();
        
        if metrics.len() < 4 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Not enough CPU metrics found"));
        }
        
        let idle = metrics[3] + metrics[4]; // idle + iowait
        
        Ok(CpuRawState {
            total: metrics.iter().sum(),
            idle,
        })
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

        // Read UID from /proc/<pid>/status
        let status_path = format!("{}/{}/status", PROC_PATH, pid);
        let mut user = String::from("?");
        if let Ok(status) = fs::read_to_string(status_path) {
            for line in status.lines() {
                if line.starts_with("Uid:") {
                    if let Some(uid_str) = line.split_whitespace().nth(1) {
                        if let Ok(uid) = uid_str.parse::<u32>() {
                            user = uid_map.get(&uid)
                                .cloned()
                                .unwrap_or_else(|| uid.to_string());
                        }
                    }
                    break;
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

        Ok((ProcessMetrics { pid, name, user, cpu_usage: 0.0, mem_res_kb }, cpu_ticks))
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
        let prev = CpuRawState { total: 100, idle: 50 };
        let cur = CpuRawState { total: 200, idle: 100 };
        // busy = (100 - 50) / 100 = 50%
        assert!((CpuRawState::calculate_usage(&prev, &cur) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn cpu_usage_no_delta_is_zero() {
        let s = CpuRawState { total: 100, idle: 50 };
        assert_eq!(CpuRawState::calculate_usage(&s, &s), 0.0);
    }

    #[test]
    fn cpu_usage_is_clamped() {
        // idle decreasing more than total still clamps into [0, 100]
        let prev = CpuRawState { total: 100, idle: 100 };
        let cur = CpuRawState { total: 110, idle: 100 };
        let u = CpuRawState::calculate_usage(&prev, &cur);
        assert!((0.0..=100.0).contains(&u));
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