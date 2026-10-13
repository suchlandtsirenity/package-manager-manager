//! What each manager has installed, and searching across all of it.

use std::time::Duration;

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::builtins;
use crate::cache::Cache;
use crate::detect::Detection;
use crate::paths::Env;
use crate::process;
use crate::recipe::{List, Recipe};

/// Readers built into pmm, usable as `[list] builtin = "..."`.
pub const BUILTINS: &[&str] = &["go-bins", "nix", "npm-globals", "nvm", "pnpm-globals"];

/// How long a list command gets. `brew list` and `snap list` can be slow.
const LIST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Human-readable name when `name` is an id, e.g. a flatpak's "Firefox"
    /// for "org.mozilla.firefox". Searched along with the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Context, not searched: the Node version an npm global belongs to, a
    /// rustup toolchain's "active, default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One manager's installed packages, or why they couldn't be read.
#[derive(Debug)]
pub struct Inventory {
    pub manager: String,
    pub items: String,
    pub packages: Result<Vec<Package>, String>,
    pub from_cache: bool,
}

/// List every installed manager that has a `[list]` section, in parallel,
/// using the cache where it's still valid. `refresh` ignores the cache.
pub fn list_all(detections: &[Detection], env: &Env, cache: &Cache, refresh: bool) -> Vec<Inventory> {
    let listable: Vec<(&Recipe, &List)> = detections
        .iter()
        .filter(|d| d.is_installed())
        .filter_map(|d| Some((&d.recipe, d.recipe.list.as_ref()?)))
        .collect();

    std::thread::scope(|s| {
        let handles: Vec<_> = listable
            .iter()
            .map(|&(recipe, list)| s.spawn(move || list_one(recipe, list, env, cache, refresh)))
            .collect();
        handles
            .into_iter()
            .zip(&listable)
            .map(|(h, (recipe, list))| {
                h.join().unwrap_or_else(|_| Inventory {
                    manager: recipe.name.clone(),
                    items: items(list),
                    packages: Err("pmm crashed while listing".into()),
                    from_cache: false,
                })
            })
            .collect()
    })
}

fn list_one(recipe: &Recipe, list: &List, env: &Env, cache: &Cache, refresh: bool) -> Inventory {
    let fingerprint = cache.fingerprint(list, env);
    let cached = if refresh { None } else { cache.get(&recipe.name, list, &fingerprint) };
    let (packages, from_cache) = match cached {
        Some(packages) => (Ok(packages), true),
        None => {
            let result = read_packages(list, env);
            if let Ok(packages) = &result {
                // A cache write failing only costs speed next time.
                let _ = cache.put(&recipe.name, list, &fingerprint, packages);
            }
            (result.map_err(|e| format!("{e:#}")), false)
        }
    };
    Inventory {
        manager: recipe.name.clone(),
        items: items(list),
        packages,
        from_cache,
    }
}

fn items(list: &List) -> String {
    list.items.clone().unwrap_or_else(|| "packages".into())
}

/// Ask the manager itself, ignoring the cache.
pub fn read_packages(list: &List, env: &Env) -> Result<Vec<Package>> {
    let mut packages = match (&list.builtin, &list.command, &list.pattern) {
        (Some(name), _, _) => builtins::run(name, env, LIST_TIMEOUT)?,
        (None, Some(command), Some(pattern)) => {
            let re = Regex::new(pattern).context("bad pattern")?;
            parse_lines(&process::run_ok(command, LIST_TIMEOUT)?, &re)
        }
        _ => anyhow::bail!("recipe has no way to list packages"),
    };
    packages.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.note.cmp(&b.note)));
    packages.dedup();
    Ok(packages)
}

/// One package per line that matches `re`; other lines are ignored.
pub fn parse_lines(output: &str, re: &Regex) -> Vec<Package> {
    let text = |caps: &regex::Captures, group| {
        caps.name(group)
            .map(|m| m.as_str().trim().to_string())
            .filter(|s| !s.is_empty())
    };
    output
        .lines()
        .filter_map(|line| re.captures(line))
        .filter_map(|caps| {
            Some(Package {
                name: text(&caps, "name")?,
                version: text(&caps, "version"),
                title: text(&caps, "title"),
                note: text(&caps, "note"),
            })
        })
        .collect()
}

/// How well a package matched a search; lower is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    Exact,
    Prefix,
    Contains,
    /// Matched the title (e.g. a flatpak's display name) but not the name.
    Title,
}

#[derive(Debug)]
pub struct Hit<'a> {
    pub manager: &'a str,
    pub package: &'a Package,
    pub rank: Rank,
}

/// Case-insensitive search of package names and notes, best matches first.
pub fn search<'a>(inventories: &'a [Inventory], query: &str) -> Vec<Hit<'a>> {
    let query = query.to_lowercase();
    let mut hits: Vec<Hit> = inventories
        .iter()
        .filter_map(|inv| Some((inv.manager.as_str(), inv.packages.as_ref().ok()?)))
        .flat_map(|(manager, packages)| packages.iter().map(move |p| (manager, p)))
        .filter_map(|(manager, package)| {
            let rank = rank(package, &query)?;
            Some(Hit { manager, package, rank })
        })
        .collect();
    hits.sort_by(|a, b| {
        (a.rank, &a.package.name, a.manager).cmp(&(b.rank, &b.package.name, b.manager))
    });
    hits
}

fn rank(package: &Package, query: &str) -> Option<Rank> {
    let name = package.name.to_lowercase();
    // For "io.github.tchx84.Flatseal" only the app part counts, or every
    // io.github.* app would match "git". For "@expo/cli", "cli" can match
    // exactly but the whole name is still searched.
    let (key, short) = match reverse_dns_app(&name) {
        Some(app) => (app, app),
        None => (name.as_str(), name.rsplit('/').next().unwrap_or(&name)),
    };
    if name == query || short == query {
        Some(Rank::Exact)
    } else if key.trim_start_matches('@').starts_with(query) || short.starts_with(query) {
        Some(Rank::Prefix)
    } else if key.contains(query) {
        Some(Rank::Contains)
    } else if package
        .title
        .as_ref()
        .is_some_and(|t| t.to_lowercase().contains(query))
    {
        Some(Rank::Title)
    } else {
        None
    }
}

/// The last part of a reverse-DNS app id like `org.mozilla.firefox`: three
/// or more parts, starting with a letters-only domain. Keeps apt names like
/// `gir1.2-gtk-3.0` out.
fn reverse_dns_app(name: &str) -> Option<&str> {
    let parts: Vec<&str> = name.split('.').collect();
    let looks_like_domain = parts[0].chars().all(|c| c.is_ascii_lowercase()) && !parts[0].is_empty();
    (parts.len() >= 3 && looks_like_domain).then(|| parts[parts.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe;

    fn pattern_of(manager: &str) -> Regex {
        let recipe = recipe::built_in().into_iter().find(|r| r.name == manager).unwrap();
        Regex::new(recipe.list.unwrap().pattern.as_deref().unwrap()).unwrap()
    }

    fn pkg(name: &str, version: Option<&str>, note: Option<&str>) -> Package {
        Package {
            name: name.into(),
            version: version.map(Into::into),
            title: None,
            note: note.map(Into::into),
        }
    }

    fn titled(name: &str, version: Option<&str>, title: &str) -> Package {
        Package { title: Some(title.into()), ..pkg(name, version, None) }
    }

    #[test]
    fn apt_skips_removed_packages() {
        let out = "ii \t1password\t8.12.40\nrc \tacpi-support\t0.144\nhi \tacpid\t1:2.0.34-1ubuntu2\n";
        assert_eq!(
            parse_lines(out, &pattern_of("apt")),
            vec![pkg("1password", Some("8.12.40"), None), pkg("acpid", Some("1:2.0.34-1ubuntu2"), None)]
        );
    }

    #[test]
    fn snap_skips_header() {
        let out = "Name    Version   Rev    Tracking       Publisher     Notes\n\
                   core22  20260824  2955   latest/stable  canonical**   base\n";
        assert_eq!(parse_lines(out, &pattern_of("snap")), vec![pkg("core22", Some("20260824"), None)]);
    }

    #[test]
    fn flatpak_keeps_display_name() {
        let out = "ai.lmstudio.lm-studio\t0.4.23-1\tLM Studio\nca.vlacroix.Tally\t\tTally\n";
        assert_eq!(
            parse_lines(out, &pattern_of("flatpak")),
            vec![
                titled("ai.lmstudio.lm-studio", Some("0.4.23-1"), "LM Studio"),
                titled("ca.vlacroix.Tally", None, "Tally"),
            ]
        );
    }

    #[test]
    fn cargo_skips_binary_lines() {
        let out = "ripgrep v14.1.0:\n    rg\nmytool v0.1.0 (/home/u/src/mytool):\n    mytool\n";
        assert_eq!(
            parse_lines(out, &pattern_of("cargo")),
            vec![pkg("ripgrep", Some("14.1.0"), None), pkg("mytool", Some("0.1.0"), Some("/home/u/src/mytool"))]
        );
    }

    #[test]
    fn uv_and_pipx_and_brew() {
        assert_eq!(
            parse_lines("ruff v0.5.0\n- ruff\nNo tools installed\n", &pattern_of("uv")),
            vec![pkg("ruff", Some("0.5.0"), None)]
        );
        assert_eq!(
            parse_lines("pywal 3.8.13\n", &pattern_of("pipx")),
            vec![pkg("pywal", Some("3.8.13"), None)]
        );
        assert_eq!(
            parse_lines("ca-certificates 2026-09-25\n", &pattern_of("brew")),
            vec![pkg("ca-certificates", Some("2026-09-25"), None)]
        );
    }

    #[test]
    fn pip_and_rustup() {
        assert_eq!(
            parse_lines("requests==2.31.0\n", &pattern_of("pip")),
            vec![pkg("requests", Some("2.31.0"), None)]
        );
        assert_eq!(
            parse_lines("stable-x86_64-unknown-linux-gnu (active, default)\nnightly-x86_64-unknown-linux-gnu\n", &pattern_of("rustup")),
            vec![
                pkg("stable-x86_64-unknown-linux-gnu", None, Some("active, default")),
                pkg("nightly-x86_64-unknown-linux-gnu", None, None),
            ]
        );
    }

    fn inv(manager: &str, packages: Vec<Package>) -> Inventory {
        Inventory {
            manager: manager.into(),
            items: "packages".into(),
            packages: Ok(packages),
            from_cache: false,
        }
    }

    #[test]
    fn search_ranks_and_spans_managers() {
        let invs = vec![
            inv("apt", vec![pkg("ripgrep", Some("13.0.0"), None), pkg("ripgrep-all", None, None), pkg("libgrep", None, None)]),
            inv("cargo", vec![pkg("ripgrep", Some("14.1.0"), None)]),
            inv("flatpak", vec![titled("org.example.Searcher", None, "Ripgrep GUI")]),
            inv("npm", vec![pkg("typescript", None, Some("ripgrep is not searched here"))]),
            Inventory { manager: "brew".into(), items: "packages".into(), packages: Err("boom".into()), from_cache: false },
        ];
        let hits = search(&invs, "RipGrep");
        let got: Vec<(&str, &str, Rank)> = hits.iter().map(|h| (h.manager, h.package.name.as_str(), h.rank)).collect();
        assert_eq!(
            got,
            vec![
                ("apt", "ripgrep", Rank::Exact),
                ("cargo", "ripgrep", Rank::Exact),
                ("apt", "ripgrep-all", Rank::Prefix),
                ("flatpak", "org.example.Searcher", Rank::Title),
            ]
        );
    }

    #[test]
    fn search_matches_last_part_of_flatpak_ids() {
        let invs = vec![inv("flatpak", vec![
            titled("org.mozilla.firefox", None, "Firefox"),
            titled("com.github.tchx84.Flatseal", None, "Flatseal"),
        ])];
        assert_eq!(search(&invs, "firefox")[0].rank, Rank::Exact);
        assert!(search(&invs, "git").is_empty(), "the io.github prefix shouldn't match");
        assert_eq!(search(&invs, "seal")[0].package.name, "com.github.tchx84.Flatseal");
    }

    #[test]
    fn search_handles_npm_scopes_and_dotted_apt_names() {
        let invs = vec![
            inv("npm", vec![pkg("@expo/ngrok", None, None)]),
            inv("apt", vec![pkg("gir1.2-gtk-3.0", None, None)]),
        ];
        assert_eq!(search(&invs, "ngrok")[0].rank, Rank::Exact);
        assert_eq!(search(&invs, "expo")[0].rank, Rank::Prefix);
        assert_eq!(search(&invs, "gtk")[0].rank, Rank::Contains);
        assert!(search(&invs, "0").iter().all(|h| h.rank != Rank::Exact));
    }
}
