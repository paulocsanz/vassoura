//! Run a child process and kill it if it stops making progress.
//! A stuck `lstat` / `git` / `lsof` cannot be cancelled inside the calling
//! thread; the daemon keeps going only if that work lives in a process
//! it is allowed to SIGKILL.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Spawn `cmd` (stdout and stderr piped) and wait up to `timeout`.
/// On timeout the child is killed and this returns `Err`.
pub fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Output, String> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        let _ = stderr.read_to_end(&mut err);
        let _ = tx.send((out, err));
    });
    let status = wait_or_kill(&mut child, timeout)?;
    let (out, err) = rx.recv().unwrap_or_default();
    Ok(Output { status, stdout: out, stderr: err })
}

/// Poll `child` until it exits. Past `timeout`, SIGKILL it and return `Err`
/// (the caller must not treat a killed child as a successful empty result).
pub fn wait_or_kill(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", timeout.as_secs().max(1)));
            }
            Ok(None) => thread::sleep(Duration::from_millis(30)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_kills_a_sleeping_child() {
        let start = Instant::now();
        let err = output_with_timeout(
            Command::new("sleep").arg("30"),
            Duration::from_millis(400),
        );
        assert!(err.is_err(), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(3), "kill did not return");
    }
}
