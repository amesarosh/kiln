//! Reading the system's state from /proc: processes, CPU, memory, disks and network.
//!
//! A `Sampler` keeps the previous reading, so rates (CPU use, bytes per second) come from the
//! difference between two samples.

use std::{collections::HashMap, fs, os::unix::fs::MetadataExt, time::Instant};

/// One process as shown in the list.
#[derive(Clone, Debug)]
pub struct Process {
    pub pid: i32,
    pub name: String,
    /// The full command line, or `[name]` for kernel threads.
    pub cmdline: String,
    pub user: String,
    pub uid: u32,
    /// Share of the whole machine's CPU time, 0-100.
    pub cpu: f32,
    /// Resident memory in bytes.
    pub memory: u64,
    pub threads: u32,
    /// The state letter from /proc (R running, S sleeping, D disk wait, Z zombie...).
    pub state: char,
    /// Has no command line: a kernel thread.
    pub kernel: bool,
}

/// The whole machine at one moment.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub processes: Vec<Process>,
    /// Total CPU use, 0-100.
    pub cpu: f32,
    /// Use of each core, 0-100.
    pub cores: Vec<f32>,
    pub mem_total: u64,
    pub mem_used: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    /// Bytes per second over all disks.
    pub disk_read: f64,
    pub disk_write: f64,
    /// Bytes per second over all network interfaces except loopback.
    pub net_rx: f64,
    pub net_tx: f64,
    pub uptime_secs: u64,
    pub load: [f32; 3],
}

/// CPU time counters from one /proc/stat line.
#[derive(Clone, Copy, Default)]
struct CpuTimes {
    busy: u64,
    total: u64,
}

impl CpuTimes {
    /// Use between `prev` and `self`, 0-100.
    fn usage_since(self, prev: CpuTimes) -> f32 {
        let total = self.total.saturating_sub(prev.total);
        if total == 0 {
            return 0.0;
        }
        (self.busy.saturating_sub(prev.busy) as f64 / total as f64 * 100.0) as f32
    }
}

pub struct Sampler {
    page_size: u64,
    users: HashMap<u32, String>,
    prev_cpu: CpuTimes,
    prev_cores: Vec<CpuTimes>,
    /// Each process's utime + stime at the last sample, keyed by pid and start time (pids are
    /// reused).
    prev_proc: HashMap<(i32, u64), u64>,
    prev_disk: (u64, u64),
    prev_net: (u64, u64),
    prev_at: Option<Instant>,
    /// Disk names that are whole devices, not partitions or loop/ram devices.
    disks: Vec<String>,
}

impl Sampler {
    pub fn new() -> Self {
        // SAFETY: sysconf has no preconditions.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        Self {
            page_size: if page_size > 0 { page_size as u64 } else { 4096 },
            users: read_users(),
            prev_cpu: CpuTimes::default(),
            prev_cores: Vec::new(),
            prev_proc: HashMap::new(),
            prev_disk: (0, 0),
            prev_net: (0, 0),
            prev_at: None,
            disks: whole_disks(),
        }
    }

    /// Read everything once. The first call has no rates yet (they read as zero).
    pub fn sample(&mut self) -> Snapshot {
        let now = Instant::now();
        let secs = self
            .prev_at
            .map(|t| now.duration_since(t).as_secs_f64())
            .filter(|s| *s > 0.0);
        let first = self.prev_at.is_none();
        self.prev_at = Some(now);

        let mut snap = Snapshot::default();
        let (cpu, cores) = read_cpu_times();
        if !first {
            snap.cpu = cpu.usage_since(self.prev_cpu);
            snap.cores = cores
                .iter()
                .enumerate()
                .map(|(i, c)| c.usage_since(self.prev_cores.get(i).copied().unwrap_or_default()))
                .collect();
        } else {
            snap.cores = vec![0.0; cores.len()];
        }
        let total_delta = cpu.total.saturating_sub(self.prev_cpu.total);
        self.prev_cpu = cpu;
        self.prev_cores = cores;

        self.read_processes(&mut snap, if first { 0 } else { total_delta });

        let mem = read_meminfo();
        let get = |k: &str| mem.get(k).copied().unwrap_or(0) * 1024;
        snap.mem_total = get("MemTotal");
        snap.mem_used = snap.mem_total.saturating_sub(get("MemAvailable"));
        snap.swap_total = get("SwapTotal");
        snap.swap_used = snap.swap_total.saturating_sub(get("SwapFree"));

        let disk = read_disk_bytes(&self.disks);
        let net = read_net_bytes();
        if let Some(s) = secs {
            snap.disk_read = disk.0.saturating_sub(self.prev_disk.0) as f64 / s;
            snap.disk_write = disk.1.saturating_sub(self.prev_disk.1) as f64 / s;
            snap.net_rx = net.0.saturating_sub(self.prev_net.0) as f64 / s;
            snap.net_tx = net.1.saturating_sub(self.prev_net.1) as f64 / s;
        }
        self.prev_disk = disk;
        self.prev_net = net;

        if let Ok(s) = fs::read_to_string("/proc/uptime") {
            snap.uptime_secs = s
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0) as u64;
        }
        if let Ok(s) = fs::read_to_string("/proc/loadavg") {
            for (i, v) in s.split_whitespace().take(3).enumerate() {
                snap.load[i] = v.parse().unwrap_or(0.0);
            }
        }
        snap
    }

    fn read_processes(&mut self, snap: &mut Snapshot, total_delta: u64) {
        let mut seen = HashMap::with_capacity(self.prev_proc.len());
        let Ok(dir) = fs::read_dir("/proc") else { return };
        for entry in dir.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
                continue;
            };
            let base = format!("/proc/{pid}");
            // A process can exit at any point; skip it if a file is gone.
            let Ok(stat) = fs::read_to_string(format!("{base}/stat")) else {
                continue;
            };
            let Some(st) = parse_stat(&stat) else { continue };
            let uid = entry.metadata().map(|m| m.uid()).unwrap_or(0);
            let memory = fs::read_to_string(format!("{base}/statm"))
                .ok()
                .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
                .unwrap_or(0)
                * self.page_size;
            let raw_cmd = fs::read(format!("{base}/cmdline")).unwrap_or_default();
            let args: Vec<String> = raw_cmd
                .split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            let kernel = args.is_empty();
            let name = display_name(&st.comm, args.first().map(String::as_str));
            let cmdline = if kernel {
                format!("[{}]", st.comm)
            } else {
                args.join(" ")
            };

            let ticks = st.utime + st.stime;
            let key = (pid, st.start);
            let cpu = match self.prev_proc.get(&key) {
                Some(prev) if total_delta > 0 => {
                    (ticks.saturating_sub(*prev) as f64 / total_delta as f64 * 100.0) as f32
                }
                _ => 0.0,
            };
            seen.insert(key, ticks);

            let user = self.user_name(uid);
            snap.processes.push(Process {
                pid,
                name,
                cmdline,
                user,
                uid,
                cpu,
                memory,
                threads: st.threads,
                state: st.state,
                kernel,
            });
        }
        self.prev_proc = seen;
    }

    fn user_name(&mut self, uid: u32) -> String {
        if !self.users.contains_key(&uid) {
            // A user added since start, or one without a passwd entry.
            self.users = read_users();
        }
        self.users.entry(uid).or_insert_with(|| uid.to_string()).clone()
    }
}

struct Stat {
    comm: String,
    state: char,
    utime: u64,
    stime: u64,
    threads: u32,
    start: u64,
}

/// Parse /proc/<pid>/stat. The name is in parentheses and may itself contain spaces and
/// parentheses, so the fields after it are found from the last ')'.
fn parse_stat(s: &str) -> Option<Stat> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = s.get(close + 2..)?.split_whitespace().collect();
    // `rest[0]` is field 3 (state); field N is rest[N - 3].
    let field = |n: usize| rest.get(n - 3).and_then(|v| v.parse::<u64>().ok());
    Some(Stat {
        comm,
        state: rest.first()?.chars().next()?,
        utime: field(14)?,
        stime: field(15)?,
        threads: field(20)? as u32,
        start: field(22)?,
    })
}

/// The kernel cuts the process name to 15 bytes; use the program's file name when it's the
/// longer form of the same name.
fn display_name(comm: &str, argv0: Option<&str>) -> String {
    if let Some(arg) = argv0 {
        let base = arg.rsplit('/').next().unwrap_or(arg);
        // Some programs put their whole command line in argv[0].
        let base = base.split(' ').next().unwrap_or(base);
        if comm.len() >= 15 && base.starts_with(comm) {
            return base.to_string();
        }
    }
    comm.to_string()
}

/// The machine's and each core's CPU counters from /proc/stat.
fn read_cpu_times() -> (CpuTimes, Vec<CpuTimes>) {
    let mut total = CpuTimes::default();
    let mut cores = Vec::new();
    let Ok(s) = fs::read_to_string("/proc/stat") else {
        return (total, cores);
    };
    for line in s.lines() {
        let mut parts = line.split_whitespace();
        let Some(label) = parts.next() else { continue };
        if !label.starts_with("cpu") {
            break;
        }
        // user nice system idle iowait irq softirq steal (guest time is already in user).
        let v: Vec<u64> = parts.take(8).filter_map(|x| x.parse().ok()).collect();
        let all: u64 = v.iter().sum();
        let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
        let times = CpuTimes {
            busy: all.saturating_sub(idle),
            total: all,
        };
        if label == "cpu" {
            total = times;
        } else {
            cores.push(times);
        }
    }
    (total, cores)
}

/// /proc/meminfo values in KiB.
fn read_meminfo() -> HashMap<String, u64> {
    let mut map = HashMap::new();
    if let Ok(s) = fs::read_to_string("/proc/meminfo") {
        for line in s.lines() {
            if let Some((k, v)) = line.split_once(':')
                && let Some(n) = v.split_whitespace().next().and_then(|n| n.parse().ok())
            {
                map.insert(k.to_string(), n);
            }
        }
    }
    map
}

fn whole_disks() -> Vec<String> {
    let Ok(dir) = fs::read_dir("/sys/block") else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| {
            !["loop", "ram", "zram", "dm-", "sr", "fd"]
                .iter()
                .any(|p| n.starts_with(p))
        })
        .collect()
}

/// Bytes read and written so far over `disks`.
fn read_disk_bytes(disks: &[String]) -> (u64, u64) {
    let Ok(s) = fs::read_to_string("/proc/diskstats") else {
        return (0, 0);
    };
    let mut read = 0;
    let mut written = 0;
    for line in s.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || !disks.iter().any(|d| d == f[2]) {
            continue;
        }
        // Sectors are always 512 bytes here, whatever the device's own sector size.
        read += f[5].parse::<u64>().unwrap_or(0) * 512;
        written += f[9].parse::<u64>().unwrap_or(0) * 512;
    }
    (read, written)
}

/// Bytes received and sent so far over every interface except loopback.
fn read_net_bytes() -> (u64, u64) {
    let Ok(s) = fs::read_to_string("/proc/net/dev") else {
        return (0, 0);
    };
    let mut rx = 0;
    let mut tx = 0;
    for line in s.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() == "lo" {
            continue;
        }
        let f: Vec<u64> = rest.split_whitespace().filter_map(|v| v.parse().ok()).collect();
        rx += f.first().copied().unwrap_or(0);
        tx += f.get(8).copied().unwrap_or(0);
    }
    (rx, tx)
}

fn read_users() -> HashMap<u32, String> {
    let mut map = HashMap::new();
    if let Ok(s) = fs::read_to_string("/etc/passwd") {
        for line in s.lines() {
            let f: Vec<&str> = line.split(':').collect();
            if let (Some(name), Some(uid)) = (f.first(), f.get(2).and_then(|u| u.parse().ok())) {
                map.insert(uid, name.to_string());
            }
        }
    }
    map
}

/// Ask a process to quit (`force` = false, SIGTERM) or stop it at once (SIGKILL).
pub fn signal(pid: i32, force: bool) -> Result<(), String> {
    let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
    // SAFETY: kill has no memory-safety preconditions.
    if unsafe { libc::kill(pid, sig) } == 0 {
        return Ok(());
    }
    Err(match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EPERM) => "it belongs to another user".to_string(),
        Some(libc::ESRCH) => "it has already exited".to_string(),
        _ => std::io::Error::last_os_error().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_odd_name() {
        let s = "1234 (my (odd) prog) S 1 1234 1234 0 -1 4194560 100 0 0 0 7 3 0 0 20 0 5 0 999 0 0";
        let st = parse_stat(s).unwrap();
        assert_eq!(st.comm, "my (odd) prog");
        assert_eq!(st.state, 'S');
        assert_eq!((st.utime, st.stime, st.threads, st.start), (7, 3, 5, 999));
    }

    #[test]
    fn long_names_come_from_argv() {
        assert_eq!(
            display_name("kiln-image-view", Some("/usr/bin/kiln-image-viewer")),
            "kiln-image-viewer"
        );
        assert_eq!(display_name("bash", Some("-bash")), "bash");
        assert_eq!(display_name("kworker/0:1", None), "kworker/0:1");
    }

    #[test]
    fn samples_this_machine() {
        let mut s = Sampler::new();
        s.sample();
        let snap = s.sample();
        assert!(snap.mem_total > 0);
        assert!(!snap.cores.is_empty());
        let me = std::process::id() as i32;
        assert!(snap.processes.iter().any(|p| p.pid == me));
    }
}
