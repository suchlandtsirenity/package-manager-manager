//! Adding and removing managers themselves, and restoring a removed
//! manager's packages. Everything here only plans; the steps are run with
//! [`crate::action::run`], so the plan can be shown (or be a `--dry-run`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::action::Step;
use crate::detect::{Detection, Status, Via};
use crate::inventory::Inventory;
use crate::manifest::{Entry, Manifest};
use crate::paths::Env;
use crate::recipe::Install;

fn find<'a>(detections: &'a [Detection], name: &str) -> Result<&'a Detection, String> {
    detections.iter().find(|d| d.recipe.name == name).ok_or_else(|| {
        let known: Vec<&str> = detections.iter().map(|d| d.recipe.name.as_str()).collect();
        format!("pmm doesn't know a manager called {name:?}. Known: {}", known.join(", "))
    })
}

/// Installing one manager.
#[derive(Debug, Clone, PartialEq)]
pub struct AddStep {
    pub manager: String,
    /// The install method's name.
    pub method: String,
    pub steps: Vec<Step>,
}

/// What `pmm add <name> [--via <method>]` would run: the managers its
/// method needs that aren't installed yet, first, then `name` itself.
/// Empty if `name` is already installed.
pub fn plan_add(name: &str, via: Option<&str>, detections: &[Detection]) -> Result<Vec<AddStep>, String> {
    let mut plan = Vec::new();
    add_into(name, via, detections, &mut plan, 0)?;
    Ok(plan)
}

fn add_into(
    name: &str,
    via: Option<&str>,
    detections: &[Detection],
    plan: &mut Vec<AddStep>,
    depth: usize,
) -> Result<(), String> {
    if depth > 8 {
        return Err(format!("installing {name} needs a manager that needs it back (a loop in the recipes)"));
    }
    let d = find(detections, name)?;
    if d.is_installed() || plan.iter().any(|a| a.manager == name) {
        return Ok(());
    }
    let methods = &d.recipe.install;
    let method = match via {
        None => methods.first().ok_or_else(|| format!("pmm doesn't know how to install {name}"))?,
        Some(via) => methods.iter().find(|m| m.name == via).ok_or_else(|| {
            let names: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();
            match names.as_slice() {
                [] => format!("pmm doesn't know how to install {name}"),
                _ => format!("{name} can't be installed via {via:?}. Ways pmm knows: {}", names.join(", ")),
            }
        })?,
    };
    if let Some(needed) = &method.manager {
        add_into(needed, None, detections, plan, depth + 1)?;
    }
    plan.push(AddStep {
        manager: name.into(),
        method: method.name.clone(),
        steps: Step::all(&method.commands, method.sudo),
    });
    Ok(())
}

/// What `pmm remove <manager>` would do.
#[derive(Debug, Clone, PartialEq)]
pub struct RemovePlan {
    pub manager: String,
    /// The install method it was matched to.
    pub method: String,
    /// Managers installed through it (directly or not), which stop working
    /// once it's gone, e.g. go when brew is removed.
    pub dependents: Vec<String>,
    /// Remove its packages, then the manager.
    pub steps: Vec<Step>,
    /// Leftovers deleted afterwards (only the ones that exist).
    pub paths: Vec<PathBuf>,
    /// Cleanup left to the user.
    pub note: Option<String>,
}

pub fn plan_remove(name: &str, detections: &[Detection], env: &Env) -> Result<RemovePlan, String> {
    let d = find(detections, name)?;
    if d.recipe.protected {
        return Err(format!("{name} is part of the system; pmm never removes it"));
    }
    let Status::Installed { via, .. } = &d.status else {
        return Err(format!("{name} isn't installed"));
    };
    let method = matching_method(&d.recipe.install, via).ok_or_else(|| match via {
        Via::Manager(m) => format!("{name} was installed via {m}, and pmm doesn't know how to remove it from there"),
        Via::Standalone => format!("pmm doesn't know how to remove {name}"),
    })?;
    let Some(uninstall) = &method.uninstall else {
        return Err(match &method.manager {
            Some(m) => format!("{name} comes with {m} and can't be removed on its own; `pmm remove {m}` removes both"),
            None => format!("pmm doesn't know how to remove {name}"),
        });
    };

    let remove = d.recipe.remove.clone().unwrap_or_default();
    let mut steps = Step::all(&remove.commands, remove.sudo);
    steps.extend(Step::all(uninstall, method.sudo));
    let paths = remove
        .paths
        .iter()
        .filter_map(|p| env.expand(p))
        .filter(|p| Some(p.as_path()) != env.home.as_deref() && fs::symlink_metadata(p).is_ok())
        .collect();
    Ok(RemovePlan {
        manager: name.into(),
        method: method.name.clone(),
        dependents: dependents(name, detections),
        steps,
        paths,
        note: remove.note,
    })
}

/// The method that installed a manager found `via` something: the one going
/// through that manager, or for a standalone install the first without one.
fn matching_method<'a>(methods: &'a [Install], via: &Via) -> Option<&'a Install> {
    match via {
        Via::Manager(m) => methods.iter().find(|i| i.manager.as_deref() == Some(m)),
        Via::Standalone => methods.iter().find(|i| i.manager.is_none()),
    }
}

/// Installed managers that were installed through `name`, directly or
/// through each other (nvm → npm → pnpm).
pub fn dependents(name: &str, detections: &[Detection]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut frontier = vec![name.to_string()];
    while let Some(parent) = frontier.pop() {
        for d in detections {
            let child = &d.recipe.name;
            let through_parent = matches!(&d.status, Status::Installed { via: Via::Manager(m), .. } if *m == parent);
            if through_parent && child != name && !found.contains(child) {
                found.push(child.clone());
                frontier.push(child.clone());
            }
        }
    }
    found.sort();
    found
}

/// Delete one leftover file or folder. A link is removed, not followed.
/// Already gone (the uninstaller may have done it) counts as done.
pub fn delete_path(path: &Path) -> io::Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) => Err(e),
    };
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// What restoring one package would do.
#[derive(Debug, Clone, PartialEq)]
pub enum Restore {
    Install(Step),
    AlreadyInstalled,
    /// Can't be restored, and why.
    Cannot(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestoreItem {
    /// The manager it goes back into.
    pub manager: String,
    pub package: String,
    pub restore: Restore,
}

/// What `pmm restore <manifest> [--via <manager>]` would do for each
/// package: reinstall it with the manager it came from, or with `via`.
pub fn plan_restore(
    manifest: &Manifest,
    via: Option<&str>,
    detections: &[Detection],
    inventories: &[Inventory],
) -> Vec<RestoreItem> {
    manifest
        .packages
        .iter()
        .map(|entry| {
            let manager = via.unwrap_or(&entry.manager).to_string();
            let restore = restore_one(&manager, entry, detections, inventories);
            RestoreItem { manager, package: entry.name.clone(), restore }
        })
        .collect()
}

fn restore_one(manager: &str, entry: &Entry, detections: &[Detection], inventories: &[Inventory]) -> Restore {
    let d = match find(detections, manager) {
        Ok(d) => d,
        Err(_) => return Restore::Cannot(format!("pmm doesn't know a manager called {manager:?}")),
    };
    if !d.is_installed() {
        return Restore::Cannot(format!("{manager} isn't installed (`pmm add {manager}`)"));
    }
    let Some(action) = &d.recipe.install_pkg else {
        return Restore::Cannot(format!("pmm can't install {manager} packages"));
    };
    // Where packages are told apart by version (nvm's "node"s), so is
    // what's already installed.
    let by_version = action.command.iter().any(|a| a.contains("{version}"));
    let installed = inventories
        .iter()
        .find(|i| i.manager == manager)
        .and_then(|i| i.packages.as_ref().ok())
        .is_some_and(|packages| {
            packages.iter().any(|p| p.name == entry.name && (!by_version || p.version == entry.version))
        });
    if installed {
        return Restore::AlreadyInstalled;
    }
    match Step::for_package(action, &entry.name, entry.version.as_deref()) {
        Some(step) => Restore::Install(step),
        None if by_version && entry.version.is_none() => Restore::Cannot("no version saved to reinstall".into()),
        None => Restore::Cannot("not a name pmm will pass to a command".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::Package;
    use crate::recipe;

    fn detection(text: &str, via: Option<Via>) -> Detection {
        Detection {
            recipe: recipe::parse(text).unwrap(),
            status: match via {
                Some(via) => Status::Installed {
                    location: PathBuf::from("/x"),
                    resolved: PathBuf::from("/x"),
                    version: None,
                    via,
                },
                None => Status::NotInstalled,
            },
        }
    }

    fn manager(name: &str, via: Option<Via>, extra: &str) -> Detection {
        detection(&format!("name = \"{name}\"\ndescription = \"x\"\n[detect]\nbinary = \"{name}\"\n{extra}"), via)
    }

    fn standalone() -> Option<Via> {
        Some(Via::Standalone)
    }

    fn via(m: &str) -> Option<Via> {
        Some(Via::Manager(m.into()))
    }

    const BREW: &str = r#"
[[install]]
name = "official"
commands = [["sh", "-c", "install brew"]]
uninstall = [["sh", "-c", "uninstall brew"]]
"#;

    const GO: &str = r#"
[[install]]
name = "brew"
manager = "brew"
commands = [["brew", "install", "go"]]
uninstall = [["brew", "uninstall", "go"]]

[[install]]
name = "apt"
manager = "apt"
commands = [["apt-get", "install", "-y", "golang-go"]]
sudo = true
uninstall = [["apt-get", "remove", "golang-go"]]

[remove]
paths = ["~/go/bin", "~/not-there"]
"#;

    #[test]
    fn add_installs_what_the_method_needs_first() {
        let detections = vec![
            manager("apt", standalone(), ""),
            manager("brew", None, BREW),
            manager("go", None, GO),
        ];
        let plan = plan_add("go", None, &detections).unwrap();
        let order: Vec<(&str, &str)> = plan.iter().map(|a| (a.manager.as_str(), a.method.as_str())).collect();
        assert_eq!(order, [("brew", "official"), ("go", "brew")]);

        let plan = plan_add("go", Some("apt"), &detections).unwrap();
        assert_eq!(plan.len(), 1, "apt is already there");
        assert!(plan[0].steps[0].sudo);

        let err = plan_add("go", Some("snap"), &detections).unwrap_err();
        assert!(err.contains("Ways pmm knows: brew, apt"), "{err}");
        assert!(plan_add("apt", None, &detections).unwrap().is_empty(), "already installed");
        assert!(plan_add("yarn", None, &detections).is_err());
    }

    #[test]
    fn remove_matches_how_it_was_installed() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join("go/bin")).unwrap();
        let env = Env { path_dirs: vec![], home: Some(home.path().to_path_buf()) };
        let mut apt = manager("apt", standalone(), "");
        apt.recipe.protected = true;
        let detections = vec![apt, manager("brew", standalone(), BREW), manager("go", via("brew"), GO)];

        let plan = plan_remove("go", &detections, &env).unwrap();
        assert_eq!(plan.method, "brew");
        assert_eq!(plan.steps, Step::all(&[vec!["brew".into(), "uninstall".into(), "go".into()]], false));
        assert_eq!(plan.paths, [home.path().join("go/bin")], "only paths that exist");

        let brew = plan_remove("brew", &detections, &env).unwrap();
        assert_eq!(brew.dependents, ["go"]);
        assert!(plan_remove("apt", &detections, &env).unwrap_err().contains("never removes"));
    }

    #[test]
    fn remove_explains_what_it_cant_do() {
        let env = Env { path_dirs: vec![], home: None };
        let cargo = "[[install]]\nname = \"rustup\"\nmanager = \"rustup\"\ncommands = [[\"rustup\", \"default\", \"stable\"]]\n";
        let detections = vec![
            manager("rustup", standalone(), BREW),
            manager("cargo", via("rustup"), cargo),
            manager("go", via("nix"), GO),
            manager("gone", None, BREW),
        ];
        let err = plan_remove("cargo", &detections, &env).unwrap_err();
        assert!(err.contains("`pmm remove rustup` removes both"), "{err}");
        let err = plan_remove("go", &detections, &env).unwrap_err();
        assert!(err.contains("installed via nix"), "{err}");
        assert!(plan_remove("gone", &detections, &env).unwrap_err().contains("isn't installed"));
    }

    #[test]
    fn dependents_are_found_through_chains() {
        let detections = vec![
            manager("nvm", standalone(), ""),
            manager("npm", via("nvm"), ""),
            manager("pnpm", via("npm"), ""),
            manager("brew", standalone(), ""),
            manager("go", via("brew"), ""),
        ];
        assert_eq!(dependents("nvm", &detections), ["npm", "pnpm"]);
        assert!(dependents("go", &detections).is_empty());
    }

    #[test]
    fn delete_path_removes_links_not_targets() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("keep");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("file"), "x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        delete_path(&link).unwrap();
        assert!(!link.exists() && target.join("file").exists());
        delete_path(&target).unwrap();
        assert!(!target.exists());
        delete_path(&target).expect("already gone is fine");
    }

    #[test]
    fn restore_plans_each_package() {
        let pipx = "[install_pkg]\ncommand = [\"pipx\", \"install\", \"{package}\"]\n";
        let detections = vec![
            manager("pipx", standalone(), pipx),
            manager("pip", standalone(), ""),
            manager("uv", None, "[install_pkg]\ncommand = [\"uv\", \"tool\", \"install\", \"{package}\"]\n"),
        ];
        let installed = Package { name: "black".into(), version: None, title: None, note: None };
        let inventories = vec![Inventory {
            manager: "pipx".into(),
            items: "packages".into(),
            packages: Ok(vec![installed]),
            from_cache: false,
        }];
        let entry = |name: &str| Entry { manager: "pip".into(), name: name.into(), version: None };
        let manifest = Manifest {
            manager: "pip".into(),
            date: "2026-10-13".into(),
            packages: vec![entry("pywal"), entry("black"), entry("x;y")],
        };

        let same = plan_restore(&manifest, None, &detections, &inventories);
        assert_eq!(same[0].restore, Restore::Cannot("pmm can't install pip packages".into()));

        let moved = plan_restore(&manifest, Some("pipx"), &detections, &inventories);
        assert!(matches!(&moved[0].restore, Restore::Install(s) if s.command == ["pipx", "install", "pywal"]));
        assert_eq!(moved[1].restore, Restore::AlreadyInstalled);
        assert!(matches!(moved[2].restore, Restore::Cannot(_)));

        let nvm = manager("nvm", standalone(), "[install_pkg]\ncommand = [\"sh\", \"-c\", \"nvm install {version}\"]\n");
        let node = |v: &str| Package { name: "node".into(), version: Some(v.into()), title: None, note: None };
        let nodes = Inventory { manager: "nvm".into(), items: "Node versions".into(), packages: Ok(vec![node("22.12.0")]), from_cache: false };
        let versions = Manifest {
            manager: "nvm".into(),
            date: "2026-10-13".into(),
            packages: ["18.17.0", "22.12.0"]
                .map(|v| Entry { manager: "nvm".into(), name: "node".into(), version: Some(v.into()) })
                .to_vec(),
        };
        let plan = plan_restore(&versions, None, &[nvm], &[nodes]);
        assert!(matches!(&plan[0].restore, Restore::Install(s) if s.command[2] == "nvm install 18.17.0"));
        assert_eq!(plan[1].restore, Restore::AlreadyInstalled);

        let missing = plan_restore(&manifest, Some("uv"), &detections, &inventories);
        assert!(matches!(&missing[0].restore, Restore::Cannot(why) if why.contains("pmm add uv")));
    }
}
