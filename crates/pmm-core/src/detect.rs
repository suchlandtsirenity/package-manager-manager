//! Working out which package managers are on this machine, which version,
//! and which other manager (if any) installed each one.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::paths::{Env, owned_depth};
use crate::process;
use crate::recipe::Recipe;

/// How long a `--version` command gets before pmm gives up on it.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Detection {
    pub recipe: Recipe,
    pub status: Status,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    NotInstalled,
    Installed {
        /// Where it was found (the PATH entry or the `detect.path` file).
        location: PathBuf,
        /// `location` with symlinks followed.
        resolved: PathBuf,
        version: Option<String>,
        via: Via,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Via {
    /// Installed by another manager, e.g. go via brew.
    Manager(String),
    /// Not inside any other manager's folders: its own installer, or by hand.
    Standalone,
}

impl Detection {
    pub fn is_installed(&self) -> bool {
        matches!(self.status, Status::Installed { .. })
    }
}

/// Detect every recipe. With `versions`, also run each manager's version
/// command (in parallel, since some take a second or more).
pub fn detect_all(recipes: &[Recipe], env: &Env, versions: bool) -> Vec<Detection> {
    let found: Vec<Option<(PathBuf, PathBuf)>> = recipes.iter().map(|r| locate(r, env)).collect();

    let versions: Vec<Option<String>> = std::thread::scope(|s| {
        let handles: Vec<_> = recipes
            .iter()
            .zip(&found)
            .map(|(recipe, found)| {
                s.spawn(move || match (found, &recipe.detect.version) {
                    (Some(_), Some(cmd)) if versions => read_version(cmd),
                    _ => None,
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().ok().flatten()).collect()
    });

    let owners: Vec<(String, Vec<PathBuf>)> = recipes
        .iter()
        .zip(&found)
        .filter(|(_, found)| found.is_some())
        .map(|(r, _)| {
            let patterns = r.detect.owns.iter().filter_map(|o| env.expand(o)).collect();
            (r.name.clone(), patterns)
        })
        .collect();

    recipes
        .iter()
        .zip(found)
        .zip(versions)
        .map(|((recipe, found), version)| {
            let status = match found {
                None => Status::NotInstalled,
                Some((location, resolved)) => {
                    let via = match infer_via(&recipe.name, &resolved, &owners) {
                        Some(name) => Via::Manager(name),
                        None => Via::Standalone,
                    };
                    Status::Installed { location, resolved, version, via }
                }
            };
            Detection { recipe: recipe.clone(), status }
        })
        .collect()
}

/// Where the manager lives, as (location, resolved), or None if absent.
fn locate(recipe: &Recipe, env: &Env) -> Option<(PathBuf, PathBuf)> {
    let location = match (&recipe.detect.binary, &recipe.detect.path) {
        (Some(bin), _) => env.find_in_path(bin)?,
        (None, Some(path)) => {
            let p = env.expand(path)?;
            p.exists().then_some(p)?
        }
        (None, None) => return None,
    };
    let resolved = location.canonicalize().unwrap_or_else(|_| location.clone());
    Some((location, resolved))
}

/// Which other manager installed `manager`, judging by where its real file
/// (`resolved`) lives. `owners` holds every installed manager's name and its
/// expanded `owns` folders. Returns None when no other manager owns the file
/// (a standalone install).
///
/// Examples from a real machine:
///   go     -> /home/linuxbrew/.linuxbrew/Cellar/go/…/bin/go              => brew
///   pnpm   -> ~/.nvm/versions/node/v22/lib/node_modules/pnpm/bin/pnpm.cjs => npm, not nvm
///   npm    -> ~/.nvm/versions/node/v22/lib/node_modules/npm/bin/npm-cli.js => nvm (npm can't install itself)
///   brew   -> /home/linuxbrew/.linuxbrew/Homebrew/bin/brew               => None
pub fn infer_via(
    manager: &str,
    resolved: &Path,
    owners: &[(String, Vec<PathBuf>)],
) -> Option<String> {
    // The deepest matching folder wins; a manager can't have installed itself.
    owners
        .iter()
        .filter(|(name, _)| name != manager)
        .filter_map(|(name, patterns)| {
            let depth = patterns.iter().filter_map(|p| owned_depth(p, resolved)).max()?;
            Some((depth, name))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, name)| name.clone())
}

static VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+\.\d+(?:\.\d+)?").expect("valid regex"));

/// Run a version command and pull the first `x.y[.z]` out of its output.
fn read_version(cmd: &[String]) -> Option<String> {
    let output = process::run(cmd, VERSION_TIMEOUT).ok()?;
    first_version(&output.stdout).or_else(|| first_version(&output.stderr))
}

pub fn first_version(text: &str) -> Option<String> {
    VERSION_RE.find(text).map(|m| m.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owners(list: &[(&str, &[&str])]) -> Vec<(String, Vec<PathBuf>)> {
        list.iter()
            .map(|(name, pats)| (name.to_string(), pats.iter().map(PathBuf::from).collect()))
            .collect()
    }

    fn machine() -> Vec<(String, Vec<PathBuf>)> {
        owners(&[
            ("apt", &["/usr", "/bin"]),
            ("brew", &["/home/linuxbrew/.linuxbrew"]),
            ("npm", &["/home/u/.nvm/versions/node/*/lib/node_modules"]),
            ("nvm", &["/home/u/.nvm"]),
            ("rustup", &["/home/u/.cargo/bin", "/home/u/.rustup"]),
            ("cargo", &[]),
        ])
    }

    fn via(manager: &str, resolved: &str) -> Option<String> {
        infer_via(manager, Path::new(resolved), &machine())
    }

    #[test]
    fn via_picks_the_owning_manager() {
        assert_eq!(via("go", "/home/linuxbrew/.linuxbrew/Cellar/go/1.27.2/libexec/bin/go").as_deref(), Some("brew"));
        assert_eq!(via("cargo", "/home/u/.cargo/bin/rustup").as_deref(), Some("rustup"));
        assert_eq!(via("pip", "/usr/bin/pip").as_deref(), Some("apt"));
    }

    #[test]
    fn via_prefers_the_most_specific_folder() {
        let pnpm = "/home/u/.nvm/versions/node/v22.12.0/lib/node_modules/pnpm/bin/pnpm.cjs";
        assert_eq!(via("pnpm", pnpm).as_deref(), Some("npm"));
    }

    #[test]
    fn via_never_names_the_manager_itself() {
        let npm = "/home/u/.nvm/versions/node/v22.12.0/lib/node_modules/npm/bin/npm-cli.js";
        assert_eq!(via("npm", npm).as_deref(), Some("nvm"));
        assert_eq!(via("brew", "/home/linuxbrew/.linuxbrew/Homebrew/bin/brew"), None);
        assert_eq!(via("rustup", "/home/u/.cargo/bin/rustup"), None);
    }

    #[test]
    fn via_is_none_outside_every_folder() {
        assert_eq!(via("uv", "/home/u/.local/bin/uv"), None);
    }

    #[test]
    fn versions_from_real_outputs() {
        let cases = [
            ("apt 2.8.3 (amd64)", "2.8.3"),
            ("go version go1.27.2 linux/amd64", "1.27.2"),
            ("pip 24.0 from /usr/lib/python3/dist-packages/pip (python 3.12)", "24.0"),
            ("nix (Nix) 2.35.2", "2.35.2"),
            ("Homebrew 4.6.3\n", "4.6.3"),
        ];
        for (output, want) in cases {
            assert_eq!(first_version(output).as_deref(), Some(want), "{output}");
        }
        assert_eq!(first_version("no version here"), None);
    }
}
