//! Working out which package managers are on this machine, which version,
//! and which other manager (if any) installed each one.

use std::collections::HashSet;
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
/// expanded `owns` folders. The deepest matching folder wins; if that's the
/// manager's own folder (ties included), it installed itself: None.
///
/// Examples from a real machine:
///   go     -> /home/linuxbrew/.linuxbrew/Cellar/go/…/bin/go               => brew
///   pnpm   -> ~/.nvm/versions/node/v22/lib/node_modules/pnpm/bin/pnpm.cjs  => npm, not nvm
///   npm    -> ~/.nvm/versions/node/v22/lib/node_modules/npm/…             => nvm (bundled with Node)
///   cargo  -> ~/.cargo/bin/rustup                                         => rustup
///   rustup -> ~/.cargo/bin/rustup  (cargo owns ~/.cargo/bin, less deeply) => None
pub fn infer_via(
    manager: &str,
    resolved: &Path,
    owners: &[(String, Vec<PathBuf>)],
) -> Option<String> {
    // Compare by depth, then prefer the manager itself (true > false).
    let (_, winner) = owners
        .iter()
        .filter_map(|(name, patterns)| {
            let depth = patterns.iter().filter_map(|p| owned_depth(p, resolved)).max()?;
            Some(((depth, name == manager), name))
        })
        .max_by_key(|(key, _)| *key)?;
    (winner != manager).then(|| winner.clone())
}

/// Who put a file on this machine: the folders every installed manager owns,
/// plus each manager's own binary (snap apps in /snap/bin are links to
/// /usr/bin/snap, which apt owns, but they belong to snap).
#[derive(Debug, Default)]
pub struct Owners {
    folders: Vec<(String, Vec<PathBuf>)>,
    /// Installed managers' binaries, where the PATH entry is the real file
    /// rather than a link to another manager's (cargo → rustup).
    binaries: Vec<(PathBuf, String)>,
    /// Files a manager keeps its own record of, for folders nobody owns
    /// (pip's scripts in ~/.local/bin).
    recorded: Vec<(HashSet<PathBuf>, String)>,
}

impl Owners {
    pub fn new(detections: &[Detection], env: &Env) -> Self {
        let mut owners = Owners::default();
        for d in detections {
            let Status::Installed { location, resolved, .. } = &d.status else {
                continue;
            };
            let patterns = d.recipe.detect.owns.iter().filter_map(|o| env.expand(o)).collect();
            owners.folders.push((d.recipe.name.clone(), patterns));
            if d.recipe.detect.binary.is_some() && location == resolved {
                owners.binaries.push((resolved.clone(), d.recipe.name.clone()));
            }
            let pip_style = d.recipe.binaries.as_ref().and_then(|b| b.builtin.as_deref()) == Some("pip-scripts");
            if pip_style {
                owners.recorded.push((crate::binaries::pip_installed_commands(env), d.recipe.name.clone()));
            }
        }
        owners
    }

    /// The manager that owns the file at `location` (e.g. a PATH entry), or
    /// None if no manager does (installed by hand, or by an unknown tool).
    pub fn owner_of(&self, location: &Path) -> Option<String> {
        let resolved = location.canonicalize().unwrap_or_else(|_| location.to_path_buf());
        if let Some((_, name)) = self.binaries.iter().find(|(bin, _)| *bin == resolved) {
            return Some(name.clone());
        }
        self.deepest(&resolved).or_else(|| self.deepest(location)).or_else(|| {
            let plain = crate::binaries::normalize_lexically(location);
            self.recorded.iter().find(|(files, _)| files.contains(&plain)).map(|(_, name)| name.clone())
        })
    }

    #[cfg(test)]
    pub(crate) fn for_tests(folders: Vec<(&str, Vec<PathBuf>)>) -> Self {
        Owners {
            folders: folders.into_iter().map(|(n, f)| (n.to_string(), f)).collect(),
            ..Default::default()
        }
    }

    fn deepest(&self, path: &Path) -> Option<String> {
        self.folders
            .iter()
            .filter_map(|(name, patterns)| {
                Some((patterns.iter().filter_map(|p| owned_depth(p, path)).max()?, name))
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, name)| name.clone())
    }
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
            ("apt", &["/usr/bin", "/bin"]),
            ("brew", &["/home/linuxbrew/.linuxbrew"]),
            ("npm", &["/home/u/.nvm/versions/node/*/lib/node_modules"]),
            ("nvm", &["/home/u/.nvm", "/home/u/.nvm/versions/node/*/lib/node_modules/npm"]),
            ("rustup", &["/home/u/.cargo/bin/rustup", "/home/u/.rustup"]),
            ("cargo", &["/home/u/.cargo/bin"]),
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
    fn via_is_none_when_own_folder_is_deepest() {
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
    fn owner_of_files_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mk = |rel: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            p
        };
        let snap_bin = mk("usr/bin/snap");
        let pipx_real = mk("home/pipx/venvs/pywal/bin/wal");
        let by_hand = mk("usr/local/bin/tool");
        std::fs::create_dir_all(root.join("snap/bin")).unwrap();
        std::fs::create_dir_all(root.join("home/bin")).unwrap();
        std::os::unix::fs::symlink(&snap_bin, root.join("snap/bin/chromium")).unwrap();
        std::os::unix::fs::symlink(&pipx_real, root.join("home/bin/wal")).unwrap();

        let owners = Owners {
            folders: vec![
                ("apt".into(), vec![root.join("usr/bin")]),
                ("snap".into(), vec![root.join("snap")]),
                ("pipx".into(), vec![root.join("home/pipx")]),
            ],
            binaries: vec![(snap_bin.clone(), "snap".into())],
            recorded: vec![(HashSet::from([root.join("home/bin/sherlock")]), "pip".into())],
        };
        let pip_script = mk("home/bin/sherlock");
        assert_eq!(owners.owner_of(&pip_script).as_deref(), Some("pip"));
        // A link to snap's own binary is a snap app, though apt owns /usr/bin.
        assert_eq!(owners.owner_of(&root.join("snap/bin/chromium")).as_deref(), Some("snap"));
        assert_eq!(owners.owner_of(&snap_bin).as_deref(), Some("snap"));
        // Judged by where the link points, not where it sits.
        assert_eq!(owners.owner_of(&root.join("home/bin/wal")).as_deref(), Some("pipx"));
        assert_eq!(owners.owner_of(&by_hand), None);
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
