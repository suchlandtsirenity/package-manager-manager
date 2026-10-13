//! Path helpers shared by detection, listing and caching: PATH lookup, `~`
//! expansion and the small `*` wildcard recipes use.

use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

/// Everything pmm needs from the environment, so tests can fake it.
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

    /// Expand a leading `~/`. Returns None if it needs a home dir we don't have.
    pub fn expand(&self, path: &str) -> Option<PathBuf> {
        match path.strip_prefix("~/") {
            Some(rest) => Some(self.home.as_ref()?.join(rest)),
            None if path == "~" => self.home.clone(),
            None => Some(PathBuf::from(path)),
        }
    }

    /// Expand `~` and `*`. A pattern without `*` gives back its one path
    /// whether or not it exists; one with `*` gives only existing matches.
    pub fn glob(&self, pattern: &str) -> Vec<PathBuf> {
        let Some(path) = self.expand(pattern) else {
            return Vec::new();
        };
        if !pattern.contains('*') {
            return vec![path];
        }
        let mut found = vec![PathBuf::new()];
        for component in path.components() {
            let part = component.as_os_str().to_string_lossy();
            if !part.contains('*') {
                found.iter_mut().for_each(|p| p.push(component));
                continue;
            }
            found = found
                .iter()
                .filter_map(|dir| std::fs::read_dir(dir).ok())
                .flatten()
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| wildcard_match(&part, &n.to_string_lossy()))
                })
                .collect();
        }
        found.retain(|p| p.symlink_metadata().is_ok());
        found.sort();
        found
    }

    pub fn find_in_path(&self, name: &str) -> Option<PathBuf> {
        find_in_path(name, &self.path_dirs)
    }
}

/// First executable called `name` in `dirs`, like `which`.
pub fn find_in_path(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| is_executable(p))
}

/// A file (after following links) with an execute bit set.
pub fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
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
        if !wildcard_match(
            &want.as_os_str().to_string_lossy(),
            &got.as_os_str().to_string_lossy(),
        ) {
            return None;
        }
    }
    Some(pattern.len())
}

pub fn wildcard_match(pattern: &str, text: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: Option<&str>) -> Env {
        Env {
            path_dirs: vec![],
            home: home.map(PathBuf::from),
        }
    }

    #[test]
    fn owned_depth_matches_whole_components() {
        assert_eq!(owned_depth(Path::new("/usr"), Path::new("/usr/bin/pip")), Some(2));
        assert_eq!(owned_depth(Path::new("/usr"), Path::new("/usrlocal/pip")), None);
        assert_eq!(owned_depth(Path::new("/a/b/c"), Path::new("/a/b")), None);
        assert_eq!(
            owned_depth(
                Path::new("/h/.local/lib/python*/site-packages"),
                Path::new("/h/.local/lib/python3.12/site-packages/x")
            ),
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
    fn expands_home() {
        assert_eq!(env(Some("/home/u")).expand("~/.nvm"), Some("/home/u/.nvm".into()));
        assert_eq!(env(Some("/home/u")).expand("/usr"), Some("/usr".into()));
        assert_eq!(env(None).expand("~/.nvm"), None);
    }

    #[test]
    fn globs_existing_matches_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for v in ["v18.17.0", "v22.12.0"] {
            std::fs::create_dir_all(root.join(".nvm/versions/node").join(v).join("lib")).unwrap();
        }
        std::fs::create_dir_all(root.join(".nvm/versions/node/other")).unwrap();
        let env = env(Some(root.to_str().unwrap()));

        let found = env.glob("~/.nvm/versions/node/v*/lib");
        assert_eq!(
            found,
            vec![
                root.join(".nvm/versions/node/v18.17.0/lib"),
                root.join(".nvm/versions/node/v22.12.0/lib"),
            ]
        );
        assert!(env.glob("~/.nvm/nothing/*").is_empty());
        // No wildcard: the path comes back even if missing.
        assert_eq!(env.glob("~/missing"), vec![root.join("missing")]);
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
