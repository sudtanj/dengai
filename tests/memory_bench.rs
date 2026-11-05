//! Memory and latency benchmark: the release binary with no Steam account, so the bot sits waiting for a
//! login on the setup page and only the HTTP API is exercised. Ignored by `cargo test`; run it with
//! `cargo test --release --test memory_bench -- --ignored --nocapture` (Linux only, it reads `/proc`).
//! `.github/workflows/bench.yml` runs it on x86_64 and arm64 runners and writes the tables to the job summary and
//! the README.
#![cfg(target_os = "linux")]

use std::fmt::Write as _;
use std::io::Write as _;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;

const BIN: &str = env!("CARGO_BIN_EXE_dengai");
const TOKEN: &str = "bench-token-bench-token";
const WARMUP: usize = 200;
const REQUESTS: usize = 5000;
const CONCURRENCY: [usize; 3] = [1, 10, 50];

/// A running dengai with its own data dir; killed and cleaned up on drop.
struct Instance {
    child: Child,
    base: String,
    data_dir: PathBuf,
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

async fn start(http: &reqwest::Client) -> Instance {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let data_dir = std::env::temp_dir().join(format!("dengai-bench-{}", std::process::id()));
    let mut cmd = Command::new(BIN);
    for (key, _) in std::env::vars() {
        if key.starts_with("DENGAI_") {
            cmd.env_remove(key);
        }
    }
    cmd.env("DENGAI_API_BIND", format!("127.0.0.1:{port}"))
        .env("DENGAI_API_TOKEN", TOKEN)
        .env("DENGAI_DATA_DIR", &data_dir)
        .env("DENGAI_LOG", "warn")
        .stdout(Stdio::null());
    let mut instance = Instance { child: cmd.spawn().unwrap(), base: format!("http://127.0.0.1:{port}"), data_dir };
    for _ in 0..1500 {
        if let Some(status) = instance.child.try_wait().unwrap() {
            panic!("dengai exited early: {status}");
        }
        if http.get(&instance.base).send().await.is_ok_and(|r| r.status().is_success()) {
            return instance;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("dengai did not become ready on {}", instance.base);
}

/// Resident and peak resident set size (KiB) and thread count, from `/proc/<pid>/status`.
struct Mem {
    rss_kb: u64,
    hwm_kb: u64,
    threads: u64,
}

fn mem(pid: u32) -> Mem {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let field = |key: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    Mem { rss_kb: field("VmRSS:"), hwm_kb: field("VmHWM:"), threads: field("Threads:") }
}

/// Polls RSS every 10 ms on its own thread and keeps the highest value seen.
struct Sampler {
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    thread: std::thread::JoinHandle<()>,
}

impl Sampler {
    fn start(pid: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let (s, p) = (stop.clone(), peak.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                p.fetch_max(mem(pid).rss_kb, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Sampler { stop, peak, thread }
    }

    fn finish(self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().unwrap();
        self.peak.load(Ordering::Relaxed)
    }
}

/// One request: `GET /status` with the token on even `i`, the open `GET /` on odd `i`.
async fn one(http: &reqwest::Client, base: &str, i: usize) -> Result<Duration, String> {
    let started = Instant::now();
    let resp = if i.is_multiple_of(2) {
        http.get(format!("{base}/status")).bearer_auth(TOKEN).send().await
    } else {
        http.get(base).send().await
    };
    let resp = resp.map_err(|e| e.to_string())?;
    let status = resp.status();
    resp.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    Ok(started.elapsed())
}

struct Run {
    concurrency: usize,
    requests: usize,
    errors: Vec<String>,
    elapsed: Duration,
    latencies: Vec<Duration>,
    peak_kb: u64,
}

impl Run {
    fn percentile(&self, p: f64) -> Duration {
        if self.latencies.is_empty() {
            return Duration::ZERO;
        }
        self.latencies[((self.latencies.len() - 1) as f64 * p).round() as usize]
    }
}

async fn load(pid: u32, http: &reqwest::Client, base: &str, concurrency: usize, requests: usize) -> Run {
    let sampler = Sampler::start(pid);
    let started = Instant::now();
    let results: Vec<Result<Duration, String>> = futures_util::stream::iter(0..requests)
        .map(|i| one(http, base, i))
        .buffer_unordered(concurrency)
        .collect()
        .await;
    let elapsed = started.elapsed();
    let peak_kb = sampler.finish();
    let (mut latencies, mut errors) = (Vec::new(), Vec::new());
    for r in results {
        match r {
            Ok(d) => latencies.push(d),
            Err(e) => errors.push(e),
        }
    }
    latencies.sort();
    Run { concurrency, requests, errors, elapsed, latencies, peak_kb }
}

fn mib(kb: u64) -> String {
    format!("{:.1} MiB", kb as f64 / 1024.0)
}

fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1000.0)
}

/// Replaces the text between `<!-- bench:{name} -->` and `<!-- /bench:{name} -->`.
fn replace_marked(text: &str, name: &str, body: &str) -> String {
    let (open, close) = (format!("<!-- bench:{name} -->"), format!("<!-- /bench:{name} -->"));
    let start = text.find(&open).unwrap_or_else(|| panic!("{open} is missing from the README")) + open.len();
    let end = start + text[start..].find(&close).unwrap_or_else(|| panic!("{close} is missing from the README"));
    format!("{}{body}{}", &text[..start], &text[end..])
}

#[test]
fn replace_marked_keeps_the_markers() {
    let text = "a <!-- bench:x -->old<!-- /bench:x --> b";
    assert_eq!(replace_marked(text, "x", "new"), "a <!-- bench:x -->new<!-- /bench:x --> b");
}

#[tokio::test]
#[ignore = "benchmark; run with --release -- --ignored"]
async fn memory_and_latency() {
    let http = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(10)).build().unwrap();
    let size_mib = std::fs::metadata(BIN).map(|m| m.len()).unwrap_or(0) as f64 / (1024.0 * 1024.0);
    let sha = std::env::var("GITHUB_SHA").unwrap_or_default();
    let commit = if sha.is_empty() { String::new() } else { format!(" at commit `{}`", &sha[..sha.len().min(7)]) };

    let started = Instant::now();
    let daemon = start(&http).await;
    let ready = started.elapsed();
    let pid = daemon.child.id();

    tokio::time::sleep(Duration::from_secs(2)).await;
    let idle = mem(pid);
    let mut runs = vec![load(pid, &http, &daemon.base, 1, WARMUP).await];
    for c in CONCURRENCY {
        runs.push(load(pid, &http, &daemon.base, c, REQUESTS).await);
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = mem(pid);

    let mut md = String::new();
    let _ = writeln!(
        md,
        "Measured on {}{commit}. Binary {size_mib:.1} MiB. There is no Steam account in the benchmark: the bot \
         waits for a login on the setup page, and the load is {REQUESTS} requests per level, alternating \
         `GET /status` (with the token) and the open `GET /`, so the numbers do not depend on Steam. Client and \
         server share the machine and the server runs on a single-threaded Tokio runtime.\n",
        std::env::consts::ARCH,
    );
    let _ = writeln!(md, "Ready {} after spawn (API accepting requests).\n", ms(ready));
    let _ = writeln!(md, "| State | RSS | Threads |\n|---|---:|---:|");
    let _ = writeln!(md, "| Idle, 2 s after start | {} | {} |", mib(idle.rss_kb), idle.threads);
    let _ = writeln!(md, "| Idle, 2 s after the load runs | {} | {} |", mib(after.rss_kb), after.threads);
    let _ = writeln!(md, "| Peak for the whole run (VmHWM) | {} | |\n", mib(after.hwm_kb));
    let _ = writeln!(md, "| Concurrency | Requests | Errors | Req/s | p50 | p99 | Peak RSS |");
    let _ = writeln!(md, "|---:|---:|---:|---:|---:|---:|---:|");
    for r in &runs {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {:.0} | {} | {} | {} |",
            r.concurrency,
            r.requests,
            r.errors.len(),
            r.latencies.len() as f64 / r.elapsed.as_secs_f64(),
            ms(r.percentile(0.5)),
            ms(r.percentile(0.99)),
            mib(r.peak_kb),
        );
    }
    let errors: Vec<&String> = runs.iter().flat_map(|r| &r.errors).collect();
    if let Some(first) = errors.first() {
        let _ = writeln!(md, "\nFirst error: `{first}`");
    }

    println!("{md}");
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") {
        let mut f = std::fs::OpenOptions::new().append(true).create(true).open(path).unwrap();
        writeln!(f, "## dengai memory benchmark\n\n{md}").unwrap();
    }
    assert!(errors.is_empty(), "some benchmark requests failed; see the report above");

    // The workflow passes BENCH_README=README.md to refresh the numbers quoted there. Each arch owns its
    // report section; the summary line quotes aarch64, the Orange Pi's arch.
    if let Ok(path) = std::env::var("BENCH_README") {
        let arch = std::env::consts::ARCH;
        let mut readme = std::fs::read_to_string(&path).unwrap();
        readme = replace_marked(&readme, &format!("report-{arch}"), &format!("\n{}\n", md.trim()));
        let last = runs.last().unwrap();
        let line = format!(
            "Measured on aarch64: a {size_mib:.1} MiB binary whose API is ready {} after start and that idles at {} RSS with {} \
             thread(s); {} concurrent API requests peak at {} (see [Benchmark](#benchmark)).",
            ms(ready),
            mib(idle.rss_kb),
            idle.threads,
            last.concurrency,
            mib(last.peak_kb),
        );
        if arch == "aarch64" {
            readme = replace_marked(&readme, "summary", &line);
        }
        std::fs::write(&path, readme).unwrap();
    }
}
