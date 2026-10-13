//! Updating everything each manager has installed. [`plan`] works out what
//! would run, so it can be shown (or be the whole of a `--dry-run`); the
//! steps are then run with [`crate::action::run`].

use crate::action::Step;
use crate::detect::Detection;
use crate::inventory::Inventory;

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
                Some(update) => Plan::Run(Step::all(&update.commands, update.sudo)),
            };
            Planned { manager: d.recipe.name.clone(), plan }
        })
        .collect()
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
}
