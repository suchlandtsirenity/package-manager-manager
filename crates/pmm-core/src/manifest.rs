//! Manifests: the packages a manager had when pmm removed it, saved to
//! `~/.local/share/pmm/removed/<manager>-<date>.toml` so `pmm restore` can
//! put them back.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::inventory::Inventory;
use crate::recipe::home_dir;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The manager that was removed.
    pub manager: String,
    /// When, as YYYY-MM-DD (UTC).
    pub date: String,
    #[serde(default, rename = "package")]
    pub packages: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Which manager it was installed with: the removed one, or one that
    /// was installed through it (cargo's crates when rustup goes).
    pub manager: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl Manifest {
    /// Everything in `inventories`, which must all have been read: a
    /// manifest missing packages would make removing them unrecoverable.
    pub fn from_inventories(manager: &str, inventories: &[&Inventory]) -> Result<Manifest, String> {
        let mut packages = Vec::new();
        for inv in inventories {
            let list = inv.packages.as_ref().map_err(|e| format!("couldn't read {}'s packages: {e}", inv.manager))?;
            packages.extend(list.iter().map(|p| Entry {
                manager: inv.manager.clone(),
                name: p.name.clone(),
                version: p.version.clone(),
            }));
        }
        Ok(Manifest { manager: manager.into(), date: today(), packages })
    }
}

/// `~/.local/share/pmm/removed`, honouring `XDG_DATA_HOME`.
pub fn default_dir() -> Option<PathBuf> {
    let data = match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home_dir()?.join(".local").join("share"),
    };
    Some(data.join("pmm").join("removed"))
}

/// Write `manifest` to `dir` as `<manager>-<date>.toml`, or `-2`, `-3`…
/// if that exists. Never overwrites.
pub fn save(dir: &Path, manifest: &Manifest) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("can't create {}", dir.display()))?;
    let text = toml::to_string_pretty(manifest)?;
    for n in 1.. {
        let suffix = if n == 1 { String::new() } else { format!("-{n}") };
        let path = dir.join(format!("{}-{}{suffix}.toml", manifest.manager, manifest.date));
        match fs::File::create_new(&path) {
            Ok(mut file) => {
                file.write_all(text.as_bytes())
                    .with_context(|| format!("can't write {}", path.display()))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("can't create {}", path.display())),
        }
    }
    unreachable!()
}

pub fn load(path: &Path) -> Result<Manifest> {
    let text = fs::read_to_string(path).with_context(|| format!("can't read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("{} isn't a pmm manifest", path.display()))
}

/// A manifest given on the command line: a path, or a name in `dir`
/// (`pipx-2026-10-13`, with or without `.toml`).
pub fn resolve(arg: &str, dir: Option<&Path>) -> PathBuf {
    let path = PathBuf::from(arg);
    if path.exists() || arg.contains('/') {
        return path;
    }
    let file = if arg.ends_with(".toml") { arg.to_string() } else { format!("{arg}.toml") };
    match dir {
        Some(dir) => dir.join(file),
        None => path,
    }
}

/// Saved manifests in `dir`, newest first. Unreadable files are skipped.
pub fn saved(dir: &Path) -> Vec<(PathBuf, Manifest)> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<(PathBuf, Manifest)> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .filter_map(|p| Some((p.clone(), load(&p).ok()?)))
        .collect();
    found.sort_by(|a, b| b.1.date.cmp(&a.1.date).then(b.0.cmp(&a.0)));
    found
}

/// Today's date (UTC) as YYYY-MM-DD.
pub fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to (year, month, day), from Howard Hinnant's
/// `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::Package;

    fn package(name: &str, version: Option<&str>) -> Package {
        Package { name: name.into(), version: version.map(Into::into), title: None, note: None }
    }

    fn inventory(manager: &str, packages: Result<Vec<Package>, String>) -> Inventory {
        Inventory { manager: manager.into(), items: "packages".into(), packages, from_cache: false }
    }

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(20_739), (2026, 10, 13));
        assert_eq!(today().len(), 10);
    }

    #[test]
    fn saves_without_overwriting_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let rustup = inventory("rustup", Ok(vec![package("stable-x86_64-unknown-linux-gnu", None)]));
        let cargo = inventory("cargo", Ok(vec![package("ripgrep", Some("14.1.0"))]));
        let mut manifest = Manifest::from_inventories("rustup", &[&rustup, &cargo]).unwrap();
        manifest.date = "2026-10-13".into();

        let first = save(dir.path(), &manifest).unwrap();
        let second = save(dir.path(), &manifest).unwrap();
        assert_eq!(first.file_name().unwrap(), "rustup-2026-10-13.toml");
        assert_eq!(second.file_name().unwrap(), "rustup-2026-10-13-2.toml");
        assert_eq!(load(&first).unwrap(), manifest);
        let text = fs::read_to_string(&first).unwrap();
        assert!(text.contains("[[package]]"), "{text}");

        assert_eq!(resolve("rustup-2026-10-13", Some(dir.path())), first);
        assert_eq!(resolve("rustup-2026-10-13.toml", Some(dir.path())), first);
        assert_eq!(saved(dir.path()).len(), 2);
    }

    #[test]
    fn refuses_incomplete_lists() {
        let broken = inventory("pipx", Err("timed out".into()));
        let err = Manifest::from_inventories("pipx", &[&broken]).unwrap_err();
        assert!(err.contains("couldn't read pipx"), "{err}");
    }
}
