//! Searching managers' repositories for packages that may not be installed,
//! and showing details about one package.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use regex::Regex;

use crate::action::is_package_name;
use crate::detect::Detection;
use crate::dupes::dupe_key;
use crate::inventory::{Inventory, Package, Rank, rank};
use crate::process;
use crate::recipe::{Recipe, SearchRemote};

/// Repositories are on the network; some (flatpak's appstream) are slow.
const TIMEOUT: Duration = Duration::from_secs(60);

/// One repository result. `package.note` holds its description.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub package: Package,
    pub rank: Rank,
    /// Already installed with this manager.
    pub installed: bool,
}

/// One manager's answer to a search.
#[derive(Debug, Clone)]
pub struct RemoteResults {
    pub manager: String,
    /// The repository only looks up exact names (PyPI).
    pub exact_only: bool,
    /// Best matches first.
    pub found: Result<Vec<Found>, String>,
    pub took: Duration,
}

/// Installed managers that can search their repository.
pub fn searchable(detections: &[Detection]) -> Vec<&Detection> {
    detections
        .iter()
        .filter(|d| d.is_installed() && d.recipe.search_remote.is_some())
        .collect()
}

/// Search every manager in `detections` that can, in parallel, calling
/// `on_results` (on this thread) as each one answers. Err if `query` isn't
/// something pmm will pass to a command.
pub fn search_remote(
    detections: &[Detection],
    inventories: &[Inventory],
    query: &str,
    mut on_results: impl FnMut(RemoteResults),
) -> Result<(), String> {
    if !is_package_name(query) {
        return Err(format!("{query:?} isn't a package name pmm can search for (letters, digits and @/._+:- only)"));
    }
    let searching = searchable(detections);
    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        for d in &searching {
            let tx = tx.clone();
            let recipe = &d.recipe;
            s.spawn(move || {
                let started = Instant::now();
                let found = search_one(recipe, query);
                let _ = tx.send((recipe, found, started.elapsed()));
            });
        }
        drop(tx);
        for (recipe, found, took) in rx {
            let installed = inventories.iter().find(|i| i.manager == recipe.name);
            on_results(RemoteResults {
                manager: recipe.name.clone(),
                exact_only: recipe.search_remote.as_ref().is_some_and(|s| s.exact),
                found: found.map(|packages| ranked(packages, query, installed)),
                took,
            });
        }
    });
    Ok(())
}

fn search_one(recipe: &Recipe, query: &str) -> Result<Vec<Package>, String> {
    let search = recipe.search_remote.as_ref().ok_or("can't search")?;
    let command: Vec<String> = search.command.iter().map(|a| a.replace("{query}", query)).collect();
    let output = process::run(&command, TIMEOUT).map_err(|e| format!("{e:#}"))?;
    let found = parse_found(&output.stdout, search)?;
    // Some searches exit non-zero with no results; only an error with
    // nothing to show is a failure.
    if !output.success && found.is_empty() {
        let reason = output.stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        if !reason.is_empty() && !reason.to_lowercase().contains("no match") {
            return Err(reason.trim().to_string());
        }
    }
    Ok(found)
}

/// Parse search output; one entry per name (brew lists a formula and a
/// cask of the same name).
pub fn parse_found(text: &str, search: &SearchRemote) -> Result<Vec<Package>, String> {
    let re = Regex::new(&search.pattern).map_err(|e| e.to_string())?;
    let mut found: Vec<Package> = Vec::new();
    for line in text.lines() {
        let Some(caps) = re.captures(line) else { continue };
        let group = |name: &str| {
            caps.name(name)
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let Some(name) = group("name") else { continue };
        if found.iter().any(|p| p.name == name) {
            continue;
        }
        found.push(Package { name, version: group("version"), title: group("title"), note: group("description") });
    }
    Ok(found)
}

/// Best matches first; otherwise the repository's own order, which is
/// usually by relevance or popularity.
fn ranked(packages: Vec<Package>, query: &str, installed: Option<&Inventory>) -> Vec<Found> {
    let query = query.to_lowercase();
    let have: Vec<String> = installed
        .and_then(|i| i.packages.as_ref().ok())
        .map(|ps| ps.iter().map(|p| dupe_key(&p.name)).collect())
        .unwrap_or_default();
    let mut found: Vec<Found> = packages
        .into_iter()
        .map(|package| Found {
            rank: rank(&package, &query).unwrap_or(Rank::Description),
            installed: have.contains(&dupe_key(&package.name)),
            package,
        })
        .collect();
    found.sort_by_key(|f| f.rank);
    found
}

/// Whether a result is the package asked for, by the same rules that spot
/// duplicates: `org.mozilla.firefox` is "firefox", `Black` is "black".
pub fn is_exact(found: &Found, query: &str) -> bool {
    dupe_key(&found.package.name) == dupe_key(query)
}

/// Details about `package` from every manager in `detections` that has an
/// `[info]` command, in parallel. Managers that don't know it are left out.
pub fn info_all<'a>(detections: &'a [Detection], package: &str) -> Vec<(&'a str, Result<String, String>)> {
    if !is_package_name(package) {
        return Vec::new();
    }
    let asking: Vec<&Detection> = detections
        .iter()
        .filter(|d| d.is_installed() && d.recipe.info.is_some())
        .collect();
    std::thread::scope(|s| {
        let handles: Vec<_> = asking.iter().map(|d| s.spawn(|| info_one(&d.recipe, package))).collect();
        asking
            .iter()
            .zip(handles)
            .map(|(d, h)| (d.recipe.name.as_str(), h.join().unwrap_or_else(|_| Err("pmm crashed".into()))))
            .collect()
    })
}

fn info_one(recipe: &Recipe, package: &str) -> Result<String, String> {
    let info = recipe.info.as_ref().ok_or("no info command")?;
    let command: Vec<String> = info.command.iter().map(|a| a.replace("{package}", package)).collect();
    let output = process::run(&command, TIMEOUT).map_err(|e| format!("{e:#}"))?;
    let text = output.stdout.trim();
    if !output.success || text.is_empty() {
        let reason = output.stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("not found");
        return Err(reason.trim().to_string());
    }
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search(pattern: &str) -> SearchRemote {
        SearchRemote { command: vec!["x".into(), "{query}".into()], pattern: pattern.into(), exact: false }
    }

    fn names(found: &[Found]) -> Vec<&str> {
        found.iter().map(|f| f.package.name.as_str()).collect()
    }

    #[test]
    fn parses_and_dedupes() {
        let brew = "==> Formulae\nripgrep\nripgrep-all\n\n==> Casks\nripgrep\n";
        let found = parse_found(brew, &search(r"^(?P<name>[A-Za-z0-9@+._/-]+)$")).unwrap();
        assert_eq!(found.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["ripgrep", "ripgrep-all"]);

        let flatpak = "org.mozilla.firefox\t158.0\tFirefox\tFast, Private & Safe Web Browser\nNo matches found\n";
        let pattern = r"^(?P<name>[^\t]+)\t(?P<version>[^\t]*)\t(?P<title>[^\t]*)\t(?P<description>.*)$";
        let found = parse_found(flatpak, &search(pattern)).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title.as_deref(), Some("Firefox"));
        assert_eq!(found[0].note.as_deref(), Some("Fast, Private & Safe Web Browser"));
    }

    #[test]
    fn ranks_and_marks_installed() {
        let p = |name: &str, title: Option<&str>| Package {
            name: name.into(),
            version: None,
            title: title.map(Into::into),
            note: Some("a search tool".into()),
        };
        let packages = vec![p("gist-search", None), p("ripgrep-edit", None), p("ripgrep", None), p("rg-wrapper", None)];
        let inv = Inventory {
            manager: "cargo".into(),
            items: "packages".into(),
            packages: Ok(vec![p("ripgrep", None)]),
            from_cache: false,
        };
        let found = ranked(packages, "ripgrep", Some(&inv));
        assert_eq!(names(&found), ["ripgrep", "ripgrep-edit", "gist-search", "rg-wrapper"]);
        assert_eq!(found[0].rank, Rank::Exact);
        assert!(found[0].installed && !found[1].installed);
        assert_eq!(found[3].rank, Rank::Description);

        let firefox = ranked(vec![p("org.mozilla.firefox", Some("Firefox"))], "firefox", None);
        assert!(is_exact(&firefox[0], "Firefox"));
    }

    #[test]
    fn refuses_odd_queries() {
        let err = search_remote(&[], &[], "a; rm", |_| {}).unwrap_err();
        assert!(err.contains("isn't a package name"));
    }
}
