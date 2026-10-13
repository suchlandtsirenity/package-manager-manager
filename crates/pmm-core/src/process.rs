//! Running a manager's command and collecting its output.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
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

/// Run `cmd` with no time limit, passing each line of stdout and stderr to
/// `on_line` as it arrives. For long jobs like updates, whose output is shown
/// live. Stdin is closed, so a command that stops to ask a question gets EOF
/// (its default answer) instead of hanging.
pub fn stream(cmd: &[String], mut on_line: impl FnMut(&str)) -> Result<ExitStatus> {
    let (program, args) = cmd.split_first().context("empty command")?;
    let mut child = Command::new(program)
        .args(args)
        .env("NO_COLOR", "1")
        .env("HOMEBREW_NO_ENV_HINTS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("can't run {program}"))?;

    let (tx, rx) = mpsc::channel::<String>();
    let readers: Vec<Box<dyn Read + Send>> = vec![
        Box::new(child.stdout.take().context("no stdout")?),
        Box::new(child.stderr.take().context("no stderr")?),
    ];
    let threads: Vec<_> = readers
        .into_iter()
        .map(|reader| {
            let tx = tx.clone();
            thread::spawn(move || {
                for line in BufReader::new(reader).split(b'\n') {
                    let Ok(line) = line else { break };
                    // Progress bars redraw a line with \r; keep its final state.
                    let text = String::from_utf8_lossy(&line);
                    let last = text.trim_end_matches('\r').rsplit('\r').next().unwrap_or("");
                    if tx.send(last.to_string()).is_err() {
                        break;
                    }
                }
            })
        })
        .collect();
    drop(tx);
    for line in rx {
        on_line(&line);
    }
    for t in threads {
        let _ = t.join();
    }
    Ok(child.wait()?)
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
    fn streams_both_outputs() {
        let mut lines = Vec::new();
        let status = stream(
            &cmd(&["sh", "-c", "echo one; echo two >&2; printf '10%%\\r50%%\\r100%%\\n'; exit 4"]),
            |l| lines.push(l.to_string()),
        )
        .unwrap();
        assert_eq!(status.code(), Some(4));
        lines.sort();
        assert_eq!(lines, ["100%", "one", "two"]);
    }

    #[test]
    fn missing_program() {
        assert!(run(&cmd(&["pmm-no-such-program"]), Duration::from_secs(1)).is_err());
    }
}
