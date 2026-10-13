//! Working out which package managers are on this machine, which version,
//! and which other manager (if any) installed each one.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use wait_timeout::ChildExt;

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

/// Everything the detector needs from the environment, so tests can fake it.
#[derive(Debug, Clone)]
pub struct Env {
    pub path_dirs: Vec<PathBuf>,
    pub home: Option<PathBuf>,
}

impl Env {
    pub fn current() -> Self {
        let path_dirs = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        Env {
            path_dirs,
            home: crate::recipe::home_dir(),
        }
    }
}

/// Detect every recipe. Version commands run in parallel, since some
/// (`brew --version`) take a second or more each.
pub fn detect_all(recipes: &[Recipe], env: &Env) -> Vec<Detection> {
    let found: Vec<Option<(PathBuf, PathBuf)>> = recipes.iter().map(|r| locate(r, env)).collect();

    let versions: Vec<Option<String>> = std::thread::scope(|s| {
        let handles: Vec<_> = recipes
            .iter()
            .zip(&found)
            .map(|(recipe, found)| {
                s.spawn(move || match (found, &recipe.detect.version) {
                    (Some(_), Some(cmd)) => read_version(cmd),
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
            let patterns = r.detect.owns.iter().filter_map(|o| expand_home(o, env)).collect();
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
        (Some(bin), _) => find_in_path(bin, &env.path_dirs)?,
        (None, Some(path)) => {
            let p = expand_home(path, env)?;
            p.exists().then_some(p)?
        }
        (None, None) => return None,
    };
    let resolved = location.canonicalize().unwrap_or_else(|_| location.clone());
    Some((location, resolved))
}

/// First executable called `name` in `dirs`, like `which`.
pub fn find_in_path(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// Expand a leading `~/`. Returns None if it needs a home dir we don't have.
pub fn expand_home(path: &str, env: &Env) -> Option<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => Some(env.home.as_ref()?.join(rest)),
        None if path == "~" => env.home.clone(),
        None => Some(PathBuf::from(path)),
    }
}

/// If `pattern` is a folder containing `path` (or is `path`), how many path
/// components it matched; None otherwise. `*` in a pattern component matches
/// any run of characters within that component, so
/// `~/.nvm/versions/node/*/lib` matches every Node version.
pub fn owned_depth(pattern: &Path, path: &Path) -> Option<usize> {
    let pattern: Vec<Component> = pattern.components().collect();
    let mut path = path.components();
    for want in &pattern {
        let got = path.next()?;
        if !wildcard_match(&want.as_os_str().to_string_lossy(), &got.as_os_str().to_string_lossy()) {
            return None;
        }
    }
    Some(pattern.len())
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let Some((first, rest)) = pattern.split_once('*') else {
        return pattern == text;
    };
    let Some(mut remaining) = text.strip_prefix(first) else {
        return false;
    };
    let mut pieces: Vec<&str> = rest.split('*').collect();
    let last = pieces.pop().unwrap_or("");
    for piece in pieces {
        match remaining.find(piece) {
            Some(i) => remaining = &remaining[i + piece.len()..],
            None => return false,
        }
    }
    remaining.len() >= last.len() && remaining.ends_with(last)
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
    let (program, args) = cmd.split_first()?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    if child.wait_timeout(VERSION_TIMEOUT).ok()?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut output);
    }
    first_version(&output)
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
    fn owned_depth_matches_whole_components() {
        assert_eq!(owned_depth(Path::new("/usr"), Path::new("/usr/bin/pip")), Some(2));
        assert_eq!(owned_depth(Path::new("/usr"), Path::new("/usrlocal/pip")), None);
        assert_eq!(owned_depth(Path::new("/a/b/c"), Path::new("/a/b")), None);
        assert_eq!(
            owned_depth(Path::new("/h/.local/lib/python*/site-packages"), Path::new("/h/.local/lib/python3.12/site-packages/x")),
            Some(6)
        );
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("python*", "python3.12"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("a*b*c", "a-x-b-y-c"));
        assert!(!wildcard_match("a*c", "abd"));
        assert!(!wildcard_match("ab*ba", "aba"));
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

    #[test]
    fn expands_home() {
        let env = Env { path_dirs: vec![], home: Some("/home/u".into()) };
        assert_eq!(expand_home("~/.nvm", &env), Some("/home/u/.nvm".into()));
        assert_eq!(expand_home("/usr", &env), Some("/usr".into()));
        let no_home = Env { path_dirs: vec![], home: None };
        assert_eq!(expand_home("~/.nvm", &no_home), None);
    }

    #[test]
    fn finds_executables_only() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("tool");
        let plain = dir.path().join("notes");
        std::fs::write(&exe, "").unwrap();
        std::fs::write(&plain, "").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dirs = vec![dir.path().to_path_buf()];
        assert_eq!(find_in_path("tool", &dirs), Some(exe));
        assert_eq!(find_in_path("notes", &dirs), None);
        assert_eq!(find_in_path("missing", &dirs), None);
    }
}
