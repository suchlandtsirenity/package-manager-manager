//! Things installed more than once: the same package under several
//! managers, and the same command in several places on PATH.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use crate::binaries::{binaries_of, normalize_python};
use crate::detect::{Detection, Owners};
use crate::inventory::{Inventory, Package, reverse_dns_app};
use crate::paths::Env;

/// One package installed under more than one manager: (manager, package).
#[derive(Debug)]
pub struct PackageDupe<'a> {
    pub copies: Vec<(&'a str, &'a Package)>,
}

/// A command and the file that runs when it's typed (first on PATH).
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub name: String,
    /// None when none of the copies' commands is on PATH at all.
    pub runs: Option<Copy>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Copy {
    pub location: PathBuf,
    /// The manager that put it there; None for files no manager owns.
    pub owner: Option<String>,
}

/// One command name found in more than one PATH folder. `copies[0]` is the
/// one that runs; the rest are hidden behind it.
#[derive(Debug, PartialEq)]
pub struct Shadowed {
    pub name: String,
    pub copies: Vec<Copy>,
}

/// The name packages are compared by: case-insensitive, `-`/`_`/`.` equal,
/// and only the app part of a reverse-DNS id, so flatpak's
/// `org.mozilla.firefox` matches apt's `firefox`.
pub fn dupe_key(name: &str) -> String {
    let lower = name.to_lowercase();
    let base = reverse_dns_app(&lower).unwrap_or(&lower);
    normalize_python(base)
}

/// Packages that more than one manager has installed, sorted by name.
pub fn package_dupes(inventories: &[Inventory]) -> Vec<PackageDupe<'_>> {
    let mut groups: BTreeMap<String, Vec<(&str, &Package)>> = BTreeMap::new();
    for inv in inventories {
        for package in inv.packages.iter().flatten() {
            groups.entry(dupe_key(&package.name)).or_default().push((&inv.manager, package));
        }
    }
    groups
        .into_values()
        .filter(|copies| {
            let mut managers: Vec<&str> = copies.iter().map(|(m, _)| *m).collect();
            managers.dedup();
            managers.len() > 1
        })
        .map(|copies| PackageDupe { copies })
        .collect()
}

/// For each command a duplicated package's copies provide, which file runs.
/// Empty when no copy's recipe can say what commands it provides.
pub fn which_runs(dupe: &PackageDupe, detections: &[Detection], owners: &Owners, env: &Env) -> Vec<Command> {
    let mut names: Vec<String> = dupe
        .copies
        .iter()
        .filter_map(|(manager, package)| {
            let recipe = &detections.iter().find(|d| d.recipe.name == *manager)?.recipe;
            binaries_of(recipe, &package.name, env)
        })
        .flatten()
        .collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| {
            let runs = env.find_in_path(&name).map(|location| Copy {
                owner: owners.owner_of(&location),
                location,
            });
            Command { name, runs }
        })
        .collect()
}

/// Commands that exist in more than one PATH folder, where the copies are
/// different files and not all from the same manager. Sorted by name.
pub fn shadowed_commands(env: &Env, owners: &Owners) -> Vec<Shadowed> {
    // Each folder once, in PATH order; /bin and /usr/bin are often the same.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for dir in &env.path_dirs {
        let real = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if !seen.contains(&real) {
            seen.push(real);
            dirs.push(dir.clone());
        }
    }

    // Name → every folder that has a file by that name, in PATH order.
    let mut by_name: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for dir in &dirs {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            by_name
                .entry(entry.file_name().to_string_lossy().into_owned())
                .or_default()
                .push(entry.path());
        }
    }

    let mut shadowed: Vec<Shadowed> = by_name
        .into_iter()
        .filter(|(_, locations)| locations.len() > 1)
        .filter_map(|(name, locations)| {
            let mut resolved_seen: Vec<PathBuf> = Vec::new();
            let copies: Vec<Copy> = locations
                .into_iter()
                .filter(|p| crate::paths::is_executable(p))
                .filter(|p| {
                    let real = p.canonicalize().unwrap_or_else(|_| p.clone());
                    let fresh = !resolved_seen.contains(&real);
                    resolved_seen.push(real);
                    fresh
                })
                .map(|location| Copy { owner: owners.owner_of(&location), location })
                .collect();
            if copies.len() < 2 {
                return None;
            }
            let all_same_manager =
                copies[1..].iter().all(|c| c.owner.is_some() && c.owner == copies[0].owner);
            (!all_same_manager).then_some(Shadowed { name, copies })
        })
        .collect();
    shadowed.sort_by(|a, b| a.name.cmp(&b.name));
    shadowed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, version: &str) -> Package {
        Package { name: name.into(), version: Some(version.into()), title: None, note: None }
    }

    fn inv(manager: &str, packages: Vec<Package>) -> Inventory {
        Inventory { manager: manager.into(), items: "packages".into(), packages: Ok(packages), from_cache: false }
    }

    #[test]
    fn keys_ignore_case_separators_and_app_id_prefixes() {
        assert_eq!(dupe_key("PyWal"), "pywal");
        assert_eq!(dupe_key("fd_find"), dupe_key("fd-find"));
        assert_eq!(dupe_key("org.mozilla.firefox"), dupe_key("firefox"));
        assert_ne!(dupe_key("gir1.2-gtk-3.0"), dupe_key("0"));
    }

    #[test]
    fn same_package_under_two_managers_only() {
        let invs = vec![
            inv("pip", vec![pkg("pywal", "3.3.0"), pkg("requests", "2.31.0")]),
            inv("pipx", vec![pkg("pywal", "3.8.13")]),
            // Same name twice in one manager (two Node versions) isn't a dupe.
            inv("npm", vec![pkg("typescript", "5.6.3"), pkg("typescript", "5.7.2")]),
        ];
        let env = Env { path_dirs: vec![], home: None };
        let dupes = package_dupes(&invs);
        assert_eq!(dupes.len(), 1);
        let managers: Vec<&str> = dupes[0].copies.iter().map(|(m, _)| *m).collect();
        assert_eq!(managers, vec!["pip", "pipx"]);
        let commands = which_runs(&dupes[0], &[], &Owners::default(), &env);
        assert!(commands.is_empty(), "no recipes, so commands are unknown");
    }

    #[test]
    fn shadowed_commands_in_path_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let exe = |rel: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            p
        };
        exe("brew/bin/bash");
        exe("usr/bin/bash");
        exe("usr/bin/only-here");
        exe("usr/bin/same");
        exe("usr/sbin/same"); // both apt: not interesting
        std::fs::write(root.join("brew/bin/notes"), "").unwrap(); // not executable
        std::fs::write(root.join("usr/bin/notes"), "").unwrap();
        std::os::unix::fs::symlink(root.join("usr/bin"), root.join("bin")).unwrap();

        let env = Env {
            path_dirs: vec![root.join("brew/bin"), root.join("usr/bin"), root.join("usr/sbin"), root.join("bin")],
            home: None,
        };
        let owners = Owners::for_tests(vec![
            ("brew", vec![root.join("brew")]),
            ("apt", vec![root.join("usr/bin"), root.join("usr/sbin")]),
        ]);
        let found = shadowed_commands(&env, &owners);
        assert_eq!(
            found,
            vec![Shadowed {
                name: "bash".into(),
                copies: vec![
                    Copy { location: root.join("brew/bin/bash"), owner: Some("brew".into()) },
                    Copy { location: root.join("usr/bin/bash"), owner: Some("apt".into()) },
                ],
            }]
        );
    }
}
