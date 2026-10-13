//! Which installed packages have a newer version, per manager, from each
//! recipe's `[outdated]` command. Updating them is [`crate::update`] (all of
//! a manager) or the recipe's `[upgrade_pkg]` (one package).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use regex::Regex;

use crate::detect::Detection;
use crate::inventory::Inventory;
use crate::process;
use crate::recipe::{Outdated, Recipe};

/// Most ask a server (brew, npm, flatpak, PyPI); cargo asks once per crate.
const TIMEOUT: Duration = Duration::from_secs(120);

/// One package with a newer version available.
#[derive(Debug, Clone, PartialEq)]
pub struct Newer {
    pub name: String,
    /// Installed version, when the command says.
    pub version: Option<String>,
    /// The version an upgrade would bring, when the command says.
    pub latest: Option<String>,
}

/// One manager's answer.
#[derive(Debug, Clone)]
pub struct Checked {
    pub manager: String,
    pub newer: Result<Vec<Newer>, String>,
    pub took: Duration,
}

/// Installed managers that can say what's outdated, leaving out ones whose
/// list is known to be empty.
pub fn checkable<'a>(detections: &'a [Detection], inventories: &[Inventory]) -> Vec<&'a Detection> {
    detections
        .iter()
        .filter(|d| d.is_installed() && d.recipe.outdated.is_some())
        .filter(|d| {
            !inventories
                .iter()
                .find(|i| i.manager == d.recipe.name)
                .is_some_and(|i| i.packages.as_ref().is_ok_and(Vec::is_empty))
        })
        .collect()
}

/// Ask every manager in `detections` that can, in parallel, calling
/// `on_checked` (on this thread) as each one answers. Installed versions
/// the command doesn't give (flatpak's) come from `inventories`.
pub fn check_all(detections: &[&Detection], inventories: &[Inventory], mut on_checked: impl FnMut(Checked)) {
    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        for d in detections {
            let tx = tx.clone();
            let recipe = &d.recipe;
            s.spawn(move || {
                let started = Instant::now();
                let newer = check(recipe);
                let _ = tx.send(Checked { manager: recipe.name.clone(), newer, took: started.elapsed() });
            });
        }
        drop(tx);
        for mut checked in rx {
            let listed = inventories.iter().find(|i| i.manager == checked.manager).and_then(|i| i.packages.as_ref().ok());
            if let (Ok(newer), Some(listed)) = (&mut checked.newer, listed) {
                for n in newer.iter_mut().filter(|n| n.version.is_none()) {
                    n.version = listed.iter().find(|p| p.name == n.name).and_then(|p| p.version.clone());
                }
            }
            on_checked(checked);
        }
    });
}

/// What one manager says is outdated, by name.
pub fn check(recipe: &Recipe) -> Result<Vec<Newer>, String> {
    let outdated = recipe.outdated.as_ref().ok_or("pmm doesn't know how to check it")?;
    let output = process::run(&outdated.command, TIMEOUT).map_err(|e| format!("{e:#}"))?;
    let mut newer = parse(&output.stdout, outdated)?;
    if !output.success && newer.is_empty() {
        let reason = output.stderr.lines().map(str::trim).find(|l| !l.is_empty());
        return Err(reason.unwrap_or("the check failed").to_string());
    }
    newer.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(newer)
}

/// One entry per name: flatpak lists a runtime once per installation.
pub fn parse(text: &str, outdated: &Outdated) -> Result<Vec<Newer>, String> {
    let re = Regex::new(&outdated.pattern).map_err(|e| e.to_string())?;
    let mut newer: Vec<Newer> = Vec::new();
    for line in text.lines() {
        let Some(caps) = re.captures(line) else { continue };
        let group = |name: &str| caps.name(name).map(|m| m.as_str().trim().to_string()).filter(|s| !s.is_empty());
        let Some(name) = group("name") else { continue };
        if newer.iter().any(|n| n.name == name) {
            continue;
        }
        newer.push(Newer { name, version: group("version"), latest: group("latest") });
    }
    Ok(newer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe;

    fn recipe_named(name: &str) -> Recipe {
        recipe::built_in().into_iter().find(|r| r.name == name).unwrap()
    }

    fn parsed(manager: &str, text: &str) -> Vec<(String, Option<String>, Option<String>)> {
        parse(text, recipe_named(manager).outdated.as_ref().unwrap())
            .unwrap()
            .into_iter()
            .map(|n| (n.name, n.version, n.latest))
            .collect()
    }

    fn row(name: &str, version: Option<&str>, latest: Option<&str>) -> (String, Option<String>, Option<String>) {
        (name.into(), version.map(Into::into), latest.map(Into::into))
    }

    #[test]
    fn parses_each_managers_output() {
        let apt = "NOTE: This is only a simulation!\n\
                   Inst libssl3t64 [3.0.13-0ubuntu3.5] (3.0.13-0ubuntu3.6 Ubuntu:24.04/noble-updates [amd64])\n\
                   Inst linux-image-7.1 (7.1.6 Pop:24.04 [amd64])\n\
                   Conf libssl3t64 (3.0.13-0ubuntu3.6 Ubuntu:24.04/noble-updates [amd64])\n";
        assert_eq!(parsed("apt", apt), [row("libssl3t64", Some("3.0.13-0ubuntu3.5"), Some("3.0.13-0ubuntu3.6"))]);

        let brew = "==> Downloading Homebrew API data\nopenssl@3 (3.6.5) < 3.6.6\nnode (22.1.0, 22.2.0) < 23.0.0\n";
        assert_eq!(
            parsed("brew", brew),
            [row("openssl@3", Some("3.6.5"), Some("3.6.6")), row("node", Some("22.1.0, 22.2.0"), Some("23.0.0"))]
        );

        let npm = "/n/lib/node_modules/@proofs-io/shotgun:@proofs-io/shotgun@0.9.2:@proofs-io/shotgun@0.7.0:@proofs-io/shotgun@0.9.2:global\n\
                   /n/lib/node_modules/typescript:typescript@7.0.2:typescript@5.7.2:typescript@7.0.2:global\n";
        assert_eq!(
            parsed("npm", npm),
            [row("@proofs-io/shotgun", Some("0.7.0"), Some("0.9.2")), row("typescript", Some("5.7.2"), Some("7.0.2"))]
        );

        let flatpak = "org.freedesktop.Platform.GL.default\t26.2.4\norg.freedesktop.Platform.GL.default\t26.2.4\norg.freedesktop.Platform.VAAPI.Intel\t\n";
        assert_eq!(
            parsed("flatpak", flatpak),
            [row("org.freedesktop.Platform.GL.default", None, Some("26.2.4")), row("org.freedesktop.Platform.VAAPI.Intel", None, None)]
        );

        let snap = "Name     Version  Rev   Size   Publisher   Notes\nfirefox  131.0    4900  250MB  mozilla**   -\n";
        assert_eq!(parsed("snap", snap), [row("firefox", None, Some("131.0"))]);
        assert!(parsed("snap", "All snaps up to date.\n").is_empty());

        let rustup = "stable-x86_64-unknown-linux-gnu - Update available : 1.99.0 (b940084d7 2026-09-28) -> 1.100.0 (c1 2026-11-09)\n\
                      nightly-x86_64-unknown-linux-gnu - up to date: 1.101.0-nightly (x 2026-10-12)\n\
                      rustup - Update available : 1.29.1 -> 1.30.0\n";
        assert_eq!(parsed("rustup", rustup), [row("stable-x86_64-unknown-linux-gnu", Some("1.99.0"), Some("1.100.0"))]);

        let tabs = "pywal\t3.8.13\t3.8.15\n";
        assert_eq!(parsed("pipx", tabs), [row("pywal", Some("3.8.13"), Some("3.8.15"))]);
        assert_eq!(parsed("cargo", "ripgrep\t14.1.0\t15.0.0\n"), [row("ripgrep", Some("14.1.0"), Some("15.0.0"))]);
        assert_eq!(parsed("uv", "ruff v0.5.0 [latest: 0.6.1]\n- ruff\n"), [row("ruff", Some("0.5.0"), Some("0.6.1"))]);
    }

    #[test]
    fn failure_with_nothing_found_is_an_error() {
        let mut r = recipe_named("npm");
        r.outdated = Some(Outdated {
            command: vec!["sh".into(), "-c".into(), "echo 'x:x@2:x@1:x@2:global'; exit 1".into()],
            pattern: r.outdated.unwrap().pattern,
        });
        assert_eq!(check(&r).unwrap().len(), 1, "npm exits 1 when it finds something");
        r.outdated.as_mut().unwrap().command[2] = "echo 'no network' >&2; exit 1".into();
        assert_eq!(check(&r).unwrap_err(), "no network");
    }
}
