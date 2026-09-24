//! Parallel scan. Each top-level directory is a job in its own process.
//! The parent kills a worker that stops emitting heartbeats and keeps
//! going with the other jobs. One wedged `lstat` cannot pin the cycle.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::config::{expand, Config, PRUNE_DIRS};
use crate::walk::{self, Class, Candidate, ScanJob};

const WORKERS: usize = 4;
const SILENCE: Duration = Duration::from_secs(20);

struct WorkerProc {
    child: Child,
    stdin: std::process::ChildStdin,
    rx: Receiver<String>,
    idle: bool,
    last_msg: Instant,
    /// Job currently assigned, so a kill can name it in the log.
    current: Option<String>,
}

pub fn run(
    exe: &Path,
    cfg: &Config,
    root_filter: Option<&Path>,
    deadline: Option<Duration>,
) -> Result<Vec<Candidate>, String> {
    let names: Vec<String> = cfg.artifact_names.clone();
    let mut jobs = walk::list_jobs(cfg, root_filter);
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    // Critical cycles rotate the queue so a slow prefix cannot starve the rest
    // forever: the next cycle starts further along.
    if deadline.is_some() {
        let shift = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as usize)
            .unwrap_or(0)
            % jobs.len();
        jobs.rotate_left(shift);
    }
    let make = || spawn_vassoura(exe, &names);
    let found = supervise(&make, jobs, WORKERS, SILENCE, deadline);
    Ok(walk::accept(cfg, found))
}

fn spawn_vassoura(exe: &Path, names: &[String]) -> Result<WorkerProc, String> {
    let mut cmd = Command::new(exe);
    cmd.arg("--scan-worker");
    spawn_cmd(cmd, names)
}

fn spawn_cmd(mut cmd: Command, names: &[String]) -> Result<WorkerProc, String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn scan worker: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("worker stdin")?;
    let stdout = child.stdout.take().ok_or("worker stdout")?;
    let cfg_line = json!({
        "op": "cfg",
        "names": names,
        "prune": PRUNE_DIRS,
    });
    writeln!(stdin, "{cfg_line}").map_err(|e| e.to_string())?;
    stdin.flush().map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line.trim_end().to_string()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok(WorkerProc {
        child,
        stdin,
        rx,
        idle: true,
        last_msg: Instant::now(),
        current: None,
    })
}

/// Shared by the real binary and by tests (tests pass a fake worker command).
fn supervise(
    make: &dyn Fn() -> Result<WorkerProc, String>,
    jobs: Vec<ScanJob>,
    workers: usize,
    silence: Duration,
    deadline: Option<Duration>,
) -> Vec<Candidate> {
    let mut queue: VecDeque<ScanJob> = jobs.into();
    let mut pool: Vec<WorkerProc> = Vec::new();
    for _ in 0..workers {
        match make() {
            Ok(w) => pool.push(w),
            Err(e) => {
                eprintln!("# warning: scan worker: {e}");
                break;
            }
        }
    }
    if pool.is_empty() {
        return Vec::new();
    }
    let mut found: HashMap<PathBuf, Candidate> = HashMap::new();
    let mut stop = false;
    let started = Instant::now();
    loop {
        if deadline.is_some_and(|d| started.elapsed() >= d) {
            stop = true;
        }
        let mut live = false;
        for w in &mut pool {
            drain_worker(w, &mut found);
            if !w.idle && w.last_msg.elapsed() >= silence {
                eprintln!(
                    "# scan worker killed (no progress for {}s): {}",
                    silence.as_secs().max(1),
                    w.current.as_deref().unwrap_or("?")
                );
                kill_worker(w);
                if let Ok(fresh) = make() {
                    *w = fresh;
                } else {
                    w.idle = true;
                }
            }
            if w.idle && !stop {
                if let Some(job) = queue.pop_front() {
                    if send_job(w, &job).is_err() {
                        eprintln!("# scan worker died before starting {}", job_label(&job));
                        kill_worker(w);
                        match make() {
                            Ok(fresh) => {
                                *w = fresh;
                                queue.push_front(job);
                            }
                            Err(e) => eprintln!("# warning: scan worker: {e}"),
                        }
                    }
                }
            }
            if !w.idle {
                live = true;
            }
        }
        if stop {
            // Deadline: keep candidates already emitted and kill the rest
            // so eviction can run. The next cycle rotates the queue.
            for w in &mut pool {
                drain_worker(w, &mut found);
                if !w.idle {
                    eprintln!(
                        "# scan worker stopped (cycle deadline): {}",
                        w.current.as_deref().unwrap_or("?")
                    );
                    kill_worker(w);
                }
            }
            break;
        }
        if !live && queue.is_empty() {
            break;
        }
        if pool.iter().all(|w| w.idle) && queue.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    for w in &mut pool {
        kill_worker(w);
    }
    found.into_values().collect()
}

fn drain_worker(w: &mut WorkerProc, found: &mut HashMap<PathBuf, Candidate>) {
    while let Ok(line) = w.rx.try_recv() {
        w.last_msg = Instant::now();
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match v.get("op").and_then(|o| o.as_str()) {
            Some("h") => {}
            Some("cand") | Some("partial") => {
                if let Some(c) = cand_from_json(&v) {
                    found.insert(c.path.clone(), c);
                }
            }
            Some("done") => {
                w.idle = true;
                w.current = None;
            }
            _ => {}
        }
    }
    if let Ok(Some(_)) = w.child.try_wait() {
        // Process left without a `done` line: abandon the job.
        if !w.idle {
            eprintln!(
                "# scan worker exited early: {}",
                w.current.as_deref().unwrap_or("?")
            );
        }
        w.idle = true;
        w.current = None;
    }
}

fn send_job(w: &mut WorkerProc, job: &ScanJob) -> std::io::Result<()> {
    let line = match job {
        ScanJob::Hunt(p) => json!({"op":"hunt","path": p.display().to_string()}),
        ScanJob::Measure { path, class } => json!({
            "op": "measure",
            "path": path.display().to_string(),
            "class": class.label(),
        }),
    };
    w.current = Some(job_label(job));
    w.idle = false;
    w.last_msg = Instant::now();
    writeln!(w.stdin, "{line}")?;
    w.stdin.flush()
}

fn kill_worker(w: &mut WorkerProc) {
    let _ = w.child.kill();
    let _ = w.child.wait();
    w.idle = true;
    w.current = None;
}

fn job_label(job: &ScanJob) -> String {
    match job {
        ScanJob::Hunt(p) | ScanJob::Measure { path: p, .. } => p.display().to_string(),
    }
}

fn cand_from_json(v: &Value) -> Option<Candidate> {
    let path = PathBuf::from(v.get("path")?.as_str()?);
    let class = if v.get("class").and_then(|c| c.as_str()) == Some("app-cache") {
        Class::AppCache
    } else {
        Class::Artifact
    };
    let newest = ns_to_time(v.get("newest_ns").and_then(|n| n.as_u64()).unwrap_or(0));
    let root = ns_to_time(v.get("root_ns").and_then(|n| n.as_u64()).unwrap_or(0));
    Some(Candidate {
        path,
        class,
        bytes: v.get("bytes").and_then(|n| n.as_u64()).unwrap_or(0),
        newest_mtime: newest,
        root_mtime: root,
        contains_git: v.get("contains_git").and_then(|b| b.as_bool()).unwrap_or(false),
        entries: v.get("entries").and_then(|n| n.as_u64()).unwrap_or(0),
    })
}

fn ns_to_time(ns: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(ns)
}

pub fn time_ns(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

/// Child entry point (`vassoura --scan-worker`). Reads jobs from stdin,
/// writes candidates and heartbeats to stdout. The parent kills this
/// process if the heartbeats stop.
pub fn worker_main() {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let Some(Ok(first)) = lines.next() else { return };
    let cfg: Value = serde_json::from_str(&first).unwrap_or(json!({}));
    let names: Vec<String> = cfg
        .get("names")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let prune: Vec<String> = cfg
        .get("prune")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_else(|| PRUNE_DIRS.iter().map(|s| s.to_string()).collect());
    let name_set: std::collections::HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
    let prune_set: std::collections::HashSet<&str> = prune.iter().map(|s| s.as_str()).collect();
    let mut prog = walk::Progress::new(true);
    let mut last_beat = Instant::now() - Duration::from_secs(10);
    let mut beat = || {
        if last_beat.elapsed() >= Duration::from_millis(500) {
            println!("{}", json!({"op":"h"}));
            let _ = std::io::stdout().flush();
            last_beat = Instant::now();
        }
    };
    for line in lines {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        beat();
        match msg.get("op").and_then(|o| o.as_str()) {
            Some("hunt") => {
                let Some(path) = msg.get("path").and_then(|p| p.as_str()) else { continue };
                let path = expand(Path::new(path));
                walk::hunt_emit(&path, &name_set, &prune_set, &mut prog, true, &mut beat, &mut |c| {
                    emit_cand("cand", &c);
                });
            }
            Some("measure") => {
                let Some(path) = msg.get("path").and_then(|p| p.as_str()) else { continue };
                let class = if msg.get("class").and_then(|c| c.as_str()) == Some("app-cache") {
                    Class::AppCache
                } else {
                    Class::Artifact
                };
                let c = walk::measure_budgeted(Path::new(path), class, &mut prog, &mut beat);
                emit_cand("cand", &c);
            }
            _ => {}
        }
        println!("{}", json!({"op":"done"}));
        let _ = std::io::stdout().flush();
    }
}

fn emit_cand(op: &str, c: &Candidate) {
    println!(
        "{}",
        json!({
            "op": op,
            "path": c.path.display().to_string(),
            "class": c.class.label(),
            "bytes": c.bytes,
            "newest_ns": time_ns(c.newest_mtime),
            "root_ns": time_ns(c.root_mtime),
            "contains_git": c.contains_git,
            "entries": c.entries,
        })
    );
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python_worker() -> Result<WorkerProc, String> {
        let mut cmd = Command::new("python3");
        cmd.arg("-c").arg(
            r#"
import sys, json, time
for line in sys.stdin:
    msg = json.loads(line)
    op = msg.get("op")
    if op == "cfg":
        continue
    path = msg.get("path", "")
    if "HANG" in path:
        sys.stdout.write(json.dumps({"op":"h"}) + "\n")
        sys.stdout.flush()
        time.sleep(60)
        continue
    sys.stdout.write(json.dumps({
        "op":"cand","path":path,"class":"artifact",
        "bytes":100,"newest_ns":0,"root_ns":0,
        "contains_git":False,"entries":1
    }) + "\n")
    sys.stdout.write(json.dumps({"op":"done"}) + "\n")
    sys.stdout.flush()
"#,
        );
        cmd.env("PYTHONUNBUFFERED", "1");
        spawn_cmd(cmd, &[])
    }

    #[test]
    fn a_silent_job_is_killed_and_the_next_job_still_runs() {
        let jobs = vec![
            ScanJob::Hunt(PathBuf::from("/tmp/HANG")),
            ScanJob::Hunt(PathBuf::from("/tmp/ok-project")),
        ];
        let start = Instant::now();
        let found = supervise(
            &python_worker,
            jobs,
            1,
            Duration::from_millis(500),
            None,
        );
        assert!(start.elapsed() < Duration::from_secs(4), "hung job pinned the scan");
        let paths: Vec<_> = found.iter().map(|c| c.path.display().to_string()).collect();
        assert!(paths.iter().any(|p| p.contains("ok-project")), "{paths:?}");
        assert!(!paths.iter().any(|p| p.contains("HANG")), "{paths:?}");
    }
}
