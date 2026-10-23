//! Remote system monitor (SPEC §5.7): one exec channel per poll on the tab's
//! own connection, no agent on the server. Linux reads /proc, macOS asks
//! top/vm_stat/sysctl/netstat, Windows runs one PowerShell command.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{remote::exec_full, ssh::Live};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Mac,
    Windows,
}

/// Raw counters from one poll.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    /// Linux: cumulative CPU jiffies (busy, total). Others: `None`.
    pub cpu_ticks: Option<(u64, u64)>,
    /// macOS / Windows: CPU % reported by the OS.
    pub cpu_pct: Option<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    /// Cumulative bytes over all non-loopback interfaces.
    pub rx: u64,
    pub tx: u64,
    pub load: Option<f32>,
    pub uptime: u64,
}

/// What the status bar shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metrics {
    pub cpu: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_pct: f32,
    /// Bytes per second.
    pub rx_rate: f64,
    pub tx_rate: f64,
    pub load: Option<f32>,
    pub uptime: u64,
}

const LINUX: &str = "echo ==stat; head -1 /proc/stat; echo ==mem; cat /proc/meminfo; echo ==net; cat /proc/net/dev; \
                     echo ==load; cat /proc/loadavg; echo ==up; cat /proc/uptime; echo ==df; df -kP /";
const MAC: &str = "echo ==top; top -l 1 -n 0 | grep '^CPU usage'; echo ==vm; vm_stat; echo ==sys; \
                   sysctl -n hw.memsize vm.loadavg kern.boottime; echo ==now; date +%s; echo ==net; netstat -ib; \
                   echo ==df; df -kP /";
const WINDOWS_PS: &str = "$c=(Get-CimInstance Win32_Processor|Measure-Object -Property LoadPercentage -Average).Average;\
$o=Get-CimInstance Win32_OperatingSystem;$d=Get-CimInstance Win32_LogicalDisk|Where-Object DeviceID -eq 'C:';\
$n=Get-NetAdapterStatistics|Measure-Object -Property ReceivedBytes,SentBytes -Sum;\
'cpu='+$c;'memtotal='+$o.TotalVisibleMemorySize;'memfree='+$o.FreePhysicalMemory;\
'disktotal='+$d.Size;'diskfree='+$d.FreeSpace;'rx='+$n[0].Sum;'tx='+$n[1].Sum;\
'uptime='+[int]((Get-Date)-$o.LastBootUpTime).TotalSeconds";

/// PowerShell with `-EncodedCommand` (UTF-16LE base64): no quoting trouble
/// whether the server's default shell is cmd or PowerShell.
fn powershell(script: &str) -> String {
    use base64::Engine as _;
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!(
        "powershell -NoProfile -NonInteractive -EncodedCommand {}",
        base64::engine::general_purpose::STANDARD.encode(utf16)
    )
}

pub fn command(os: Os) -> String {
    match os {
        Os::Linux => LINUX.into(),
        Os::Mac => MAC.into(),
        Os::Windows => powershell(WINDOWS_PS),
    }
}

/// `==name` separated sections.
fn sections(out: &str) -> impl Iterator<Item = (&str, &str)> {
    out.split("==").filter_map(|s| s.split_once('\n'))
}

fn num(s: &str) -> u64 {
    s.trim().trim_end_matches('.').parse().unwrap_or(0)
}

/// `df -kP /` → (used, total) bytes.
fn df(body: &str) -> (u64, u64) {
    body.lines()
        .nth(1)
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (
                f.get(2).map_or(0, |v| num(v)) * 1024,
                f.get(1).map_or(0, |v| num(v)) * 1024,
            )
        })
        .unwrap_or_default()
}

pub fn parse_linux(out: &str) -> Sample {
    let mut s = Sample::default();
    for (name, body) in sections(out) {
        match name.trim() {
            "stat" => {
                let v: Vec<u64> = body.split_whitespace().skip(1).map(num).collect();
                if v.len() >= 4 {
                    // user nice system idle iowait irq softirq steal
                    let idle = v[3] + v.get(4).copied().unwrap_or(0);
                    let total: u64 = v.iter().take(8).sum();
                    s.cpu_ticks = Some((total - idle, total));
                }
            }
            "mem" => {
                let field = |k: &str| {
                    body.lines()
                        .find_map(|l| l.strip_prefix(k))
                        .map_or(0, |v| num(v.trim().trim_end_matches("kB")) * 1024)
                };
                s.mem_total = field("MemTotal:");
                s.mem_used = s.mem_total.saturating_sub(field("MemAvailable:"));
            }
            "net" => {
                for l in body.lines().skip(2) {
                    let Some((iface, rest)) = l.split_once(':') else {
                        continue;
                    };
                    if iface.trim() == "lo" {
                        continue;
                    }
                    let f: Vec<u64> = rest.split_whitespace().map(num).collect();
                    s.rx += f.first().copied().unwrap_or(0);
                    s.tx += f.get(8).copied().unwrap_or(0);
                }
            }
            "load" => s.load = body.split_whitespace().next().and_then(|v| v.parse().ok()),
            "up" => {
                s.uptime = body
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0) as u64
            }
            "df" => (s.disk_used, s.disk_total) = df(body),
            _ => {}
        }
    }
    s
}

pub fn parse_mac(out: &str) -> Sample {
    let mut s = Sample::default();
    let mut boot = 0;
    let mut now = 0;
    for (name, body) in sections(out) {
        match name.trim() {
            // CPU usage: 5.12% user, 3.40% sys, 91.48% idle
            "top" => {
                s.cpu_pct = body
                    .split(',')
                    .find(|p| p.contains("idle"))
                    .and_then(|p| p.split('%').next())
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .map(|idle| 100.0 - idle);
            }
            "vm" => {
                let page = body
                    .split("page size of ")
                    .nth(1)
                    .and_then(|r| r.split_whitespace().next())
                    .map_or(4096, num);
                let pages = |k: &str| {
                    body.lines()
                        .find_map(|l| l.strip_prefix(k))
                        .map_or(0, |v| num(v.trim_start_matches(':')))
                };
                // Like Activity Monitor's "Memory Used": app + wired + compressed.
                s.mem_used =
                    (pages("Pages active") + pages("Pages wired down") + pages("Pages occupied by compressor")) * page;
            }
            "sys" => {
                let mut lines = body.lines();
                s.mem_total = lines.next().map_or(0, num);
                s.load = lines
                    .next()
                    .and_then(|l| {
                        l.trim_matches(|c| c == '{' || c == '}' || c == ' ')
                            .split_whitespace()
                            .next()
                    })
                    .and_then(|v| v.parse().ok());
                boot = lines
                    .next()
                    .and_then(|l| l.split("sec = ").nth(1))
                    .map_or(0, |v| num(v.split(',').next().unwrap_or("")));
            }
            "now" => now = num(body),
            "net" => {
                for l in body.lines().skip(1) {
                    let f: Vec<&str> = l.split_whitespace().collect();
                    // Name Mtu Network Address Ipkts Ierrs Ibytes Opkts Oerrs Obytes Coll
                    if f.len() < 7 || f[0].starts_with("lo") || !f[2].starts_with("<Link#") {
                        continue;
                    }
                    let n = f.len();
                    s.rx += num(f[n - 5]);
                    s.tx += num(f[n - 2]);
                }
            }
            "df" => (s.disk_used, s.disk_total) = df(body),
            _ => {}
        }
    }
    s.uptime = now.saturating_sub(boot);
    s
}

pub fn parse_windows(out: &str) -> Sample {
    let v = |k: &str| {
        out.lines()
            .find_map(|l| l.trim().strip_prefix(k)?.strip_prefix('='))
            .map_or(0, |v| num(v.split('.').next().unwrap_or("")))
    };
    let (mem_total, mem_free) = (v("memtotal") * 1024, v("memfree") * 1024);
    let (disk_total, disk_free) = (v("disktotal"), v("diskfree"));
    Sample {
        cpu_pct: Some(v("cpu") as f32),
        mem_total,
        mem_used: mem_total.saturating_sub(mem_free),
        disk_total,
        disk_used: disk_total.saturating_sub(disk_free),
        rx: v("rx"),
        tx: v("tx"),
        uptime: v("uptime"),
        ..Default::default()
    }
}

/// Rates and CPU from two consecutive samples `secs` apart.
pub fn metrics(prev: &Sample, cur: &Sample, secs: f64) -> Metrics {
    let cpu = match (prev.cpu_ticks, cur.cpu_ticks, cur.cpu_pct) {
        (_, _, Some(p)) => p,
        (Some((b0, t0)), Some((b1, t1)), _) if t1 > t0 => (b1.saturating_sub(b0)) as f32 * 100.0 / (t1 - t0) as f32,
        _ => 0.0,
    };
    let rate = |a: u64, b: u64| {
        if secs > 0.0 {
            b.saturating_sub(a) as f64 / secs
        } else {
            0.0
        }
    };
    Metrics {
        cpu: cpu.clamp(0.0, 100.0),
        mem_used: cur.mem_used,
        mem_total: cur.mem_total,
        disk_pct: if cur.disk_total > 0 {
            cur.disk_used as f32 * 100.0 / cur.disk_total as f32
        } else {
            0.0
        },
        rx_rate: rate(prev.rx, cur.rx),
        tx_rate: rate(prev.tx, cur.tx),
        load: cur.load,
        uptime: cur.uptime,
    }
}

/// Shared between a tab and its monitor task.
pub struct Shared {
    pub metrics: Mutex<Option<Metrics>>,
    /// Gave up after 3 failed polls.
    pub unavailable: AtomicBool,
    epoch: Instant,
    /// Last time (ms since `epoch`) the tab was drawn.
    seen_ms: AtomicU64,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            metrics: Mutex::new(None),
            unavailable: AtomicBool::new(false),
            epoch: Instant::now(),
            seen_ms: AtomicU64::new(0),
        }
    }
}

impl Shared {
    /// The tab is on screen; polling pauses 3 s after the last call.
    pub fn touch(&self) {
        self.seen_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn visible(&self) -> bool {
        (self.epoch.elapsed().as_millis() as u64).saturating_sub(self.seen_ms.load(Ordering::Relaxed)) < 3000
    }
}

/// Polls while the tab exists (the task holds the only other `Arc`).
pub async fn run(live: Live, shared: Arc<Shared>) {
    let mut os = None;
    let mut prev: Option<(Sample, Instant)> = None;
    let mut fails = 0;
    loop {
        let every = if os == Some(Os::Windows) { 5 } else { 2 };
        tokio::time::sleep(Duration::from_secs(every)).await;
        if Arc::strong_count(&shared) == 1 {
            return;
        }
        let h = live.lock().ok().and_then(|l| l.clone());
        let Some(h) = h.filter(|_| shared.visible()) else {
            continue;
        };
        let os = match os {
            Some(o) => o,
            None => match detect(&h).await {
                Some(o) => *os.insert(o),
                None => {
                    fails += 1;
                    if fails >= 3 {
                        shared.unavailable.store(true, Ordering::Relaxed);
                        return;
                    }
                    continue;
                }
            },
        };
        let r = exec_full(&h, &command(os)).await;
        let sample = match r {
            Ok(r) if !r.stdout.trim().is_empty() => match os {
                Os::Linux => parse_linux(&r.stdout),
                Os::Mac => parse_mac(&r.stdout),
                Os::Windows => parse_windows(&r.stdout),
            },
            _ => {
                fails += 1;
                if fails >= 3 {
                    shared.unavailable.store(true, Ordering::Relaxed);
                    return;
                }
                continue;
            }
        };
        fails = 0;
        let now = Instant::now();
        if let Some((p, at)) = &prev
            && let Ok(mut m) = shared.metrics.lock()
        {
            *m = Some(metrics(p, &sample, now.duration_since(*at).as_secs_f64()));
        }
        prev = Some((sample, now));
    }
}

async fn detect(h: &crate::ssh::SshHandle) -> Option<Os> {
    if let Ok(r) = exec_full(h, "uname -s").await {
        match r.stdout.trim() {
            "Linux" => return Some(Os::Linux),
            "Darwin" => return Some(Os::Mac),
            _ => {}
        }
    }
    let r = exec_full(h, &powershell("'win'")).await.ok()?;
    (r.stdout.trim() == "win").then_some(Os::Windows)
}

/// `1.2 GB`, `340 KB`.
pub fn bytes(n: f64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i >= 3 || (v < 10.0 && i > 0) {
        format!("{v:.1} {}", units[i])
    } else {
        format!("{v:.0} {}", units[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINUX_OUT: &str = "==stat
cpu  100 0 50 800 50 0 0 0 0 0
==mem
MemTotal:        2000000 kB
MemFree:          500000 kB
MemAvailable:    1500000 kB
==net
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  999999     100    0    0    0     0          0         0   999999     100    0    0    0     0       0          0
  eth0: 1000000    2000    0    0    0     0          0         0   500000    1000    0    0    0     0       0          0
==load
0.52 0.40 0.30 1/200 1234
==up
3600.50 7000.00
==df
Filesystem     1024-blocks    Used Available Capacity Mounted on
overlay           1000000  250000    750000      25% /
";

    #[test]
    fn linux() {
        let a = parse_linux(LINUX_OUT);
        assert_eq!(a.cpu_ticks, Some((150, 1000)));
        assert_eq!(a.mem_total, 2_048_000_000);
        assert_eq!(a.mem_used, 512_000_000);
        assert_eq!((a.rx, a.tx), (1_000_000, 500_000));
        assert_eq!(a.load, Some(0.52));
        assert_eq!(a.uptime, 3600);
        assert_eq!((a.disk_used, a.disk_total), (256_000_000, 1_024_000_000));
        let mut b = a.clone();
        b.cpu_ticks = Some((250, 1200));
        b.rx += 2000;
        let m = metrics(&a, &b, 2.0);
        assert_eq!(m.cpu, 50.0);
        assert_eq!(m.rx_rate, 1000.0);
        assert_eq!(m.disk_pct, 25.0);
    }

    #[test]
    fn mac() {
        let out = "==top
CPU usage: 5.12% user, 3.40% sys, 91.48% idle
==vm
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                               10000.
Pages active:                            100000.
Pages wired down:                         50000.
Pages occupied by compressor:             10000.
==sys
17179869184
{ 1.85 2.10 2.30 }
{ sec = 1760000000, usec = 123456 } Thu Oct  9 10:00:00 2025
==now
1760003600
==net
Name       Mtu   Network       Address            Ipkts Ierrs     Ibytes    Opkts Oerrs     Obytes  Coll
lo0        16384 <Link#1>                        1000     0     50000     1000     0     50000     0
en0        1500  <Link#11>   aa:bb:cc:dd:ee:ff  5000     0   3000000     4000     0    700000     0
en0        1500  192.168.1     192.168.1.5        5000     -   3000000     4000     -    700000     -
utun0      1380  <Link#20>                          10     0       100       10     0        200     0
==df
Filesystem   1024-blocks      Used Available Capacity  Mounted on
/dev/disk3s1s1  971350180  10000000 400000000     3%    /
";
        let s = parse_mac(out);
        assert!((s.cpu_pct.unwrap_or(0.0) - 8.52).abs() < 0.01);
        assert_eq!(s.mem_used, 160_000 * 16384);
        assert_eq!(s.mem_total, 17_179_869_184);
        assert_eq!(s.load, Some(1.85));
        assert_eq!(s.uptime, 3600);
        assert_eq!((s.rx, s.tx), (3_000_100, 700_200));
        assert_eq!(s.disk_used, 10_000_000 * 1024);
    }

    #[test]
    fn windows() {
        let out = "cpu=17\r\nmemtotal=16777216\r\nmemfree=8388608\r\ndisktotal=500000000000\r\ndiskfree=250000000000\r\nrx=123456\r\ntx=654321\r\nuptime=86400\r\n";
        let s = parse_windows(out);
        assert_eq!(s.cpu_pct, Some(17.0));
        assert_eq!(s.mem_used, 8_589_934_592);
        assert_eq!(s.disk_used, 250_000_000_000);
        assert_eq!((s.rx, s.tx, s.uptime), (123_456, 654_321, 86_400));
        assert!(command(Os::Windows).starts_with("powershell -NoProfile"));
    }

    #[test]
    fn units() {
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(1536.0), "1.5 KB");
        assert_eq!(bytes(3.0 * 1024.0 * 1024.0 * 1024.0), "3.0 GB");
    }
}
