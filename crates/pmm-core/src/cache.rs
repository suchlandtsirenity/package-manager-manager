//! Cached package lists in `~/.cache/pmm/list/<manager>.json`.
//!
//! A cached list is reused while the recipe's `watch` paths look exactly as
//! they did when it was saved (same set of paths, same modification times)
//! and it isn't older than the time limit. Recipes without `watch` paths only
//! get the short limit.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::inventory::Package;
use crate::paths::Env;
use crate::recipe::List;

/// Longest a list is trusted when its watch paths haven't changed. Catches
/// changes the watch paths miss.
const MAX_AGE_WATCHED: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_AGE_UNWATCHED: Duration = Duration::from_secs(10 * 60);

/// Bump when the file format changes, so old caches are ignored.
const FORMAT: u32 = 2;

pub struct Cache {
    dir: Option<PathBuf>,
}

/// Each watched path and its modification time (None if it doesn't exist).
pub type Fingerprint = Vec<(PathBuf, Option<u128>)>;

#[derive(Serialize, Deserialize)]
struct Entry {
    format: u32,
    /// The recipe's `[list]` section, so editing a recipe invalidates it.
    list: String,
    saved_at: u64,
    watch: Fingerprint,
    packages: Vec<Package>,
}

impl Cache {
    /// Cache in `dir`, or a cache that never stores anything if None.
    pub fn new(dir: Option<PathBuf>) -> Self {
        Cache { dir }
    }

    /// `~/.cache/pmm/list`, honouring `XDG_CACHE_HOME`.
    pub fn default_dir() -> Option<PathBuf> {
        let base = match std::env::var_os("XDG_CACHE_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => crate::recipe::home_dir()?.join(".cache"),
        };
        Some(base.join("pmm").join("list"))
    }

    pub fn fingerprint(&self, list: &List, env: &Env) -> Fingerprint {
        let mut paths: Vec<PathBuf> = list.watch.iter().flat_map(|w| env.glob(w)).collect();
        paths.sort();
        paths.dedup();
        paths
            .into_iter()
            .map(|p| {
                // symlink_metadata: ~/.nix-profile is a link into the read-only
                // store, whose files all have the same 1970 timestamp.
                let mtime = p
                    .symlink_metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos());
                (p, mtime)
            })
            .collect()
    }

    pub fn get(&self, manager: &str, list: &List, fingerprint: &Fingerprint) -> Option<Vec<Package>> {
        let text = fs::read_to_string(self.file(manager)?).ok()?;
        let entry: Entry = serde_json::from_str(&text).ok()?;
        let max_age = if list.watch.is_empty() { MAX_AGE_UNWATCHED } else { MAX_AGE_WATCHED };
        let age = now().saturating_sub(entry.saved_at);
        let fresh = entry.format == FORMAT
            && entry.list == list_key(list)
            && &entry.watch == fingerprint
            && age < max_age.as_secs();
        fresh.then_some(entry.packages)
    }

    pub fn put(&self, manager: &str, list: &List, fingerprint: &Fingerprint, packages: &[Package]) -> Result<()> {
        let Some(file) = self.file(manager) else {
            return Ok(());
        };
        let entry = Entry {
            format: FORMAT,
            list: list_key(list),
            saved_at: now(),
            watch: fingerprint.clone(),
            packages: packages.to_vec(),
        };
        let dir = file.parent().context("cache file has no folder")?;
        fs::create_dir_all(dir)?;
        // Write then rename, so a parallel pmm never reads half a file.
        let tmp = dir.join(format!(".{manager}.{}.tmp", std::process::id()));
        fs::write(&tmp, serde_json::to_vec(&entry)?)?;
        fs::rename(&tmp, &file)?;
        Ok(())
    }

    /// When the cached list for `manager` was saved, if there is one.
    pub fn saved_at(&self, manager: &str) -> Option<SystemTime> {
        let text = fs::read_to_string(self.file(manager)?).ok()?;
        let entry: Entry = serde_json::from_str(&text).ok()?;
        Some(UNIX_EPOCH + Duration::from_secs(entry.saved_at))
    }

    fn file(&self, manager: &str) -> Option<PathBuf> {
        Some(self.dir.as_deref()?.join(format!("{manager}.json")))
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }
}

/// The newest modification time among the watched paths: roughly when the
/// manager last installed, removed or upgraded something. None if no watched
/// path exists.
pub fn last_change(fingerprint: &Fingerprint) -> Option<SystemTime> {
    let nanos = fingerprint.iter().filter_map(|(_, mtime)| *mtime).max()?;
    Some(UNIX_EPOCH + Duration::from_nanos(u64::try_from(nanos).ok()?))
}

fn list_key(list: &List) -> String {
    serde_json::to_string(list).unwrap_or_default()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(watch: &[&str]) -> List {
        List {
            command: Some(vec!["true".into()]),
            pattern: Some("(?P<name>.*)".into()),
            builtin: None,
            items: None,
            watch: watch.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn pkgs() -> Vec<Package> {
        vec![Package { name: "ripgrep".into(), version: Some("14.1.0".into()), title: None, note: None }]
    }

    #[test]
    fn reuses_until_a_watched_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("status");
        fs::write(&watched, "a").unwrap();
        let env = Env { path_dirs: vec![], home: None };
        let cache = Cache::new(Some(dir.path().join("cache")));
        let list = list(&[watched.to_str().unwrap()]);

        let fp = cache.fingerprint(&list, &env);
        cache.put("apt", &list, &fp, &pkgs()).unwrap();
        assert_eq!(cache.get("apt", &list, &cache.fingerprint(&list, &env)), Some(pkgs()));

        // Simulate an install touching the watched file.
        let later = SystemTime::now() + Duration::from_secs(5);
        fs::File::options().write(true).open(&watched).unwrap().set_modified(later).unwrap();
        assert_eq!(cache.get("apt", &list, &cache.fingerprint(&list, &env)), None);
    }

    #[test]
    fn watched_file_appearing_or_vanishing_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("crates2.json");
        let env = Env { path_dirs: vec![], home: None };
        let cache = Cache::new(Some(dir.path().join("cache")));
        let list = list(&[watched.to_str().unwrap()]);

        let fp = cache.fingerprint(&list, &env);
        cache.put("cargo", &list, &fp, &pkgs()).unwrap();
        fs::write(&watched, "{}").unwrap();
        assert_eq!(cache.get("cargo", &list, &cache.fingerprint(&list, &env)), None);
    }

    #[test]
    fn editing_the_recipe_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env { path_dirs: vec![], home: None };
        let cache = Cache::new(Some(dir.path().to_path_buf()));
        let old = list(&[]);
        let fp = cache.fingerprint(&old, &env);
        cache.put("x", &old, &fp, &pkgs()).unwrap();
        assert!(cache.get("x", &old, &fp).is_some());

        let mut new = old.clone();
        new.pattern = Some("^(?P<name>\\S+)".into());
        assert_eq!(cache.get("x", &new, &fp), None);
    }

    #[test]
    fn disabled_cache_stores_nothing() {
        let cache = Cache::new(None);
        let l = list(&[]);
        cache.put("x", &l, &vec![], &pkgs()).unwrap();
        assert_eq!(cache.get("x", &l, &vec![]), None);
    }
}
