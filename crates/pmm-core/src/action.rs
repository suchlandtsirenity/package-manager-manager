//! Running a list of commands that change the system (updates, adding and
//! removing managers, installing packages), reporting progress as
//! [`Event`]s so the CLI can print them and the GUI can show them live.

use std::time::{Duration, Instant};

use crate::process;
use crate::recipe;

/// One command to run.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub command: Vec<String>,
    /// Needs root; [`Step::argv`] adds the elevation program.
    pub sudo: bool,
}

impl Step {
    /// One step per command, all with the same `sudo`.
    pub fn all(commands: &[Vec<String>], sudo: bool) -> Vec<Step> {
        commands.iter().map(|c| Step { command: c.clone(), sudo }).collect()
    }

    /// A recipe's per-package command with `{package}` and `{version}`
    /// filled in. `None` if a value has characters that don't belong in a
    /// package name (names can come from a hand-edited manifest, and some
    /// commands are shell scripts), or `{version}` is needed but unknown.
    pub fn for_package(action: &recipe::PackageAction, package: &str, version: Option<&str>) -> Option<Step> {
        if !is_package_name(package) || version.is_some_and(|v| !is_package_name(v)) {
            return None;
        }
        let mut command = Vec::with_capacity(action.command.len());
        for arg in &action.command {
            let arg = arg.replace("{package}", package);
            command.push(if arg.contains("{version}") { arg.replace("{version}", version?) } else { arg });
        }
        Some(Step { command, sudo: action.sudo })
    }

    /// The command as run, prefixed with `elevate` (`sudo`, or `pkexec` in
    /// the GUI) when it needs root.
    pub fn argv(&self, elevate: &str) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.command.len() + 1);
        if self.sudo {
            argv.push(elevate.to_string());
        }
        argv.extend(self.command.iter().cloned());
        argv
    }

    /// The command as a person would type it, e.g. `sudo apt-get update`.
    pub fn shown(&self, elevate: &str) -> String {
        self.argv(elevate).iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
    }
}

/// Letters, digits and `@/._+:-`: enough for npm scopes, flatpak ids,
/// toolchains and versions, nothing a shell would interpret, and no `..`
/// (some commands build a path from the name).
pub fn is_package_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.contains("..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "@/._+:-".contains(c))
}

/// Shell-style quoting for display only; nothing is run through a shell.
fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// Where a command's output goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Io {
    /// Captured and passed on line by line as [`Event::Output`]; stdin is
    /// closed, so prompts take their default. For unattended runs (updates).
    Stream,
    /// Straight to the terminal, so the command can ask questions (apt's
    /// "these other packages will also be removed, continue?"). No
    /// [`Event::Output`] events.
    Terminal,
}

/// Progress of [`run`].
#[derive(Debug)]
pub enum Event<'a> {
    /// A step is starting.
    Step(&'a Step),
    /// A line of its output (with [`Io::Stream`]).
    Output(&'a str),
}

/// Why a run failed.
#[derive(Debug, Clone, PartialEq)]
pub struct Failed {
    pub step: Step,
    pub reason: String,
}

/// Run `steps` in order, stopping at the first that fails: later steps
/// usually depend on earlier ones (no upgrade after a failed refresh, no
/// removing a manager whose packages couldn't be removed).
pub fn run(steps: &[Step], elevate: &str, io: Io, mut on_event: impl FnMut(Event)) -> Result<Duration, Failed> {
    let started = Instant::now();
    for step in steps {
        on_event(Event::Step(step));
        let fail = |reason: String| Failed { step: step.clone(), reason };
        let argv = step.argv(elevate);
        let status = match io {
            Io::Stream => process::stream(&argv, |line| on_event(Event::Output(line))),
            Io::Terminal => process::interactive(&argv),
        }
        .map_err(|e| fail(format!("{e:#}")))?;
        if !status.success() {
            return Err(fail(match status.code() {
                Some(code) => format!("exited with code {code}"),
                None => "was stopped by a signal".into(),
            }));
        }
    }
    Ok(started.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sudo_is_added_and_shown() {
        let step = Step { command: cmd(&["apt-get", "-y", "upgrade"]), sudo: true };
        assert_eq!(step.argv("pkexec"), cmd(&["pkexec", "apt-get", "-y", "upgrade"]));
        assert_eq!(step.shown("sudo"), "sudo apt-get -y upgrade");
        let script = Step { command: cmd(&["sh", "-c", "echo 'hi' there"]), sudo: false };
        assert_eq!(script.shown("sudo"), r"sh -c 'echo '\''hi'\'' there'");
    }

    #[test]
    fn package_names_are_checked_before_filling_in() {
        let action = recipe::PackageAction { command: cmd(&["npm", "install", "-g", "{package}"]), sudo: false };
        let step = Step::for_package(&action, "@scope/tool", None).unwrap();
        assert_eq!(step.command, cmd(&["npm", "install", "-g", "@scope/tool"]));
        for bad in ["", "-rf", "a b", "x;rm", "$(boom)", "a'b", "../../.bashrc"] {
            assert!(Step::for_package(&action, bad, None).is_none(), "{bad:?}");
        }
        let nvm = recipe::PackageAction { command: cmd(&["sh", "-c", "nvm install {version}"]), sudo: false };
        let step = Step::for_package(&nvm, "node", Some("22.12.0")).unwrap();
        assert_eq!(step.command[2], "nvm install 22.12.0");
        assert!(Step::for_package(&nvm, "node", None).is_none());
        assert!(Step::for_package(&nvm, "node", Some("1;reboot")).is_none());
        assert!(is_package_name("org.gnome.Calculator"));
        assert!(is_package_name("stable-x86_64-unknown-linux-gnu"));
        assert!(is_package_name("v22.12.0"));
    }

    #[test]
    fn run_stops_at_first_failure() {
        let steps = vec![
            Step { command: cmd(&["sh", "-c", "echo first"]), sudo: false },
            Step { command: cmd(&["sh", "-c", "exit 3"]), sudo: false },
            Step { command: cmd(&["sh", "-c", "echo never"]), sudo: false },
        ];
        let mut output = Vec::new();
        let mut started = 0;
        let err = run(&steps, "sudo", Io::Stream, |e| match e {
            Event::Step(_) => started += 1,
            Event::Output(line) => output.push(line.to_string()),
        })
        .unwrap_err();
        assert_eq!(started, 2);
        assert_eq!(output, ["first"]);
        assert_eq!(err.step, steps[1]);
        assert_eq!(err.reason, "exited with code 3");
    }

    #[test]
    fn run_reports_missing_program() {
        let steps = vec![Step { command: cmd(&["pmm-no-such-program"]), sudo: false }];
        let err = run(&steps, "sudo", Io::Terminal, |_| {}).unwrap_err();
        assert!(err.reason.contains("can't run"), "{}", err.reason);
    }
}
