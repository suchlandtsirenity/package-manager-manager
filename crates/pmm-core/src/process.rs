//! Running a manager's command and collecting its output.

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use wait_timeout::ChildExt;

#[derive(Debug)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run `cmd` (program + args), giving up after `timeout`. Output is read on
/// separate threads: `dpkg-query` prints far more than a pipe holds, and
/// waiting before reading would deadlock.
pub fn run(cmd: &[String], timeout: Duration) -> Result<Output> {
    let (program, args) = cmd.split_first().context("empty command")?;
    let mut child = Command::new(program)
        .args(args)
        // Stable, uncoloured output that the recipe patterns can rely on.
        .env("LC_ALL", "C.UTF-8")
        .env("NO_COLOR", "1")
        .env("HOMEBREW_NO_ENV_HINTS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("can't run {program}"))?;

    let mut stdout = child.stdout.take().context("no stdout")?;
    let mut stderr = child.stderr.take().context("no stderr")?;
    let out = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });

    let Some(status) = child.wait_timeout(timeout)? else {
        let _ = child.kill();
        let _ = child.wait();
        bail!("{program} didn't finish within {}s", timeout.as_secs());
    };
    let text = |handle: thread::JoinHandle<Vec<u8>>| {
        String::from_utf8_lossy(&handle.join().unwrap_or_default()).into_owned()
    };
    Ok(Output {
        success: status.success(),
        stdout: text(out),
        stderr: text(err),
    })
}

/// Like [`run`], but a non-zero exit is an error carrying the first line of
/// stderr.
pub fn run_ok(cmd: &[String], timeout: Duration) -> Result<String> {
    let output = run(cmd, timeout)?;
    if !output.success {
        let reason = output
            .stderr
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no error message");
        bail!("`{}` failed: {reason}", cmd.join(" "));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn collects_large_output() {
        let out = run_ok(&cmd(&["sh", "-c", "seq 1 200000"]), Duration::from_secs(10)).unwrap();
        assert_eq!(out.lines().count(), 200000);
    }

    #[test]
    fn reports_failure_reason() {
        let err = run_ok(&cmd(&["sh", "-c", "echo boom >&2; exit 3"]), Duration::from_secs(5))
            .unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn times_out() {
        let err = run(&cmd(&["sleep", "5"]), Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("didn't finish"));
    }

    #[test]
    fn missing_program() {
        assert!(run(&cmd(&["pmm-no-such-program"]), Duration::from_secs(1)).is_err());
    }
}
