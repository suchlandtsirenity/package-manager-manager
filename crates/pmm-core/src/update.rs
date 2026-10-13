//! Updating everything each manager has installed.
//!
//! [`plan`] works out what would run, so it can be shown (or be the whole
//! of a `--dry-run`); [`run`] runs one manager's steps and reports progress
//! as [`Event`]s, so the CLI can print them and the GUI can show them live.

use std::time::{Duration, Instant};

use crate::detect::Detection;
use crate::inventory::Inventory;
use crate::process;

/// One command of an update.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub command: Vec<String>,
    /// Needs root; [`Step::argv`] adds the elevation program.
    pub sudo: bool,
}

impl Step {
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

#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    Run(Vec<Step>),
    Skip(Skip),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Skip {
    /// Its recipe has no `[update]` section.
    NoCommand,
    /// Its list is empty, so there's nothing to update.
    NothingInstalled,
}

impl Skip {
    pub fn reason(self) -> &'static str {
        match self {
            Skip::NoCommand => "pmm doesn't know how to update it",
            Skip::NothingInstalled => "nothing installed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub manager: String,
    pub plan: Plan,
}

impl Planned {
    pub fn needs_sudo(&self) -> bool {
        matches!(&self.plan, Plan::Run(steps) if steps.iter().any(|s| s.sudo))
    }
}

/// What an update of every installed manager in `detections` would run.
/// Protected (system) managers go first, the rest by name. A manager whose
/// list is known to be empty is skipped; one whose list couldn't be read is
/// updated anyway.
pub fn plan(detections: &[Detection], inventories: &[Inventory]) -> Vec<Planned> {
    let mut installed: Vec<&Detection> = detections.iter().filter(|d| d.is_installed()).collect();
    installed.sort_by_key(|d| (!d.recipe.protected, d.recipe.name.clone()));
    installed
        .into_iter()
        .map(|d| {
            let empty = inventories
                .iter()
                .find(|i| i.manager == d.recipe.name)
                .is_some_and(|i| i.packages.as_ref().is_ok_and(Vec::is_empty));
            let plan = match &d.recipe.update {
                None => Plan::Skip(Skip::NoCommand),
                Some(_) if empty => Plan::Skip(Skip::NothingInstalled),
                Some(update) => Plan::Run(
                    update
                        .commands
                        .iter()
                        .map(|c| Step { command: c.clone(), sudo: update.sudo })
                        .collect(),
                ),
            };
            Planned { manager: d.recipe.name.clone(), plan }
        })
        .collect()
}

/// Progress of [`run`].
#[derive(Debug)]
pub enum Event<'a> {
    /// A step is starting.
    Step(&'a Step),
    /// A line of its output.
    Output(&'a str),
}

/// Why a manager's update failed.
#[derive(Debug, Clone, PartialEq)]
pub struct Failed {
    pub step: Step,
    pub reason: String,
}

/// Run `steps` in order, stopping at the first that fails (an upgrade after
/// a failed refresh would only fail again or use stale data).
pub fn run(steps: &[Step], elevate: &str, mut on_event: impl FnMut(Event)) -> Result<Duration, Failed> {
    let started = Instant::now();
    for step in steps {
        on_event(Event::Step(step));
        let fail = |reason: String| Failed { step: step.clone(), reason };
        let status = process::stream(&step.argv(elevate), |line| on_event(Event::Output(line)))
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
    use std::path::PathBuf;

    use super::*;
    use crate::detect::{Status, Via};
    use crate::inventory::Package;
    use crate::recipe;

    fn detection(text: &str) -> Detection {
        Detection {
            recipe: recipe::parse(text).unwrap(),
            status: Status::Installed {
                location: PathBuf::from("/x"),
                resolved: PathBuf::from("/x"),
                version: None,
                via: Via::Standalone,
            },
        }
    }

    fn manager(name: &str, extra: &str) -> Detection {
        detection(&format!(
            "name = \"{name}\"\ndescription = \"x\"\n[detect]\nbinary = \"{name}\"\n{extra}"
        ))
    }

    fn inventory(manager: &str, packages: Result<Vec<Package>, String>) -> Inventory {
        Inventory { manager: manager.into(), items: "packages".into(), packages, from_cache: false }
    }

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plans_protected_first_and_skips() {
        let update = "[update]\ncommands = [[\"up\"]]\n";
        let mut not_installed = manager("gone", update);
        not_installed.status = Status::NotInstalled;
        let mut apt = manager("apt", "[update]\ncommands = [[\"apt-get\", \"update\"]]\nsudo = true\n");
        apt.recipe.protected = true;
        let detections = vec![
            manager("brew", update),
            manager("cargo", update),
            manager("pip", ""),
            apt,
            manager("flatpak", update),
            not_installed,
        ];
        let inventories = vec![
            inventory("cargo", Ok(vec![])),
            inventory("flatpak", Err("broken".into())),
        ];
        let planned = plan(&detections, &inventories);
        let names: Vec<&str> = planned.iter().map(|p| p.manager.as_str()).collect();
        assert_eq!(names, ["apt", "brew", "cargo", "flatpak", "pip"]);
        assert!(planned[0].needs_sudo());
        assert!(!planned[1].needs_sudo());
        assert_eq!(planned[2].plan, Plan::Skip(Skip::NothingInstalled));
        assert!(matches!(planned[3].plan, Plan::Run(_)), "unreadable list still updates");
        assert_eq!(planned[4].plan, Plan::Skip(Skip::NoCommand));
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
    fn run_stops_at_first_failure() {
        let steps = vec![
            Step { command: cmd(&["sh", "-c", "echo first"]), sudo: false },
            Step { command: cmd(&["sh", "-c", "exit 3"]), sudo: false },
            Step { command: cmd(&["sh", "-c", "echo never"]), sudo: false },
        ];
        let mut output = Vec::new();
        let mut started = 0;
        let err = run(&steps, "sudo", |e| match e {
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
        let err = run(&steps, "sudo", |_| {}).unwrap_err();
        assert!(err.reason.contains("can't run"), "{}", err.reason);
    }
}
