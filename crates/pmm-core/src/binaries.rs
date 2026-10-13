//! The commands one package provides (`ripgrep` → `rg`), from a recipe's
//! `[binaries]` section. Only asked for a handful of packages at a time (the
//! duplicated ones), so each lookup can afford to read files or run a command.

use std::fs;
use std::path::Path;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use crate::paths::Env;
use crate::process;
use crate::recipe::{Binaries, Recipe};

/// Lookups built into pmm, usable as `[binaries] builtin = "..."`.
pub const BUILTINS: &[&str] = &["cargo-bins", "npm-bins", "pip-scripts", "pipx-apps", "uv-tools"];

const TIMEOUT: Duration = Duration::from_secs(30);

/// Command names `package` provides under `recipe`'s manager, sorted. None
/// when the recipe has no `[binaries]` section or the lookup failed.
pub fn binaries_of(recipe: &Recipe, package: &str, env: &Env) -> Option<Vec<String>> {
    let b = recipe.binaries.as_ref()?;
    let mut names = lookup(b, package, env)?;
    names.sort();
    names.dedup();
    Some(names)
}

fn lookup(b: &Binaries, package: &str, env: &Env) -> Option<Vec<String>> {
    if let Some(builtin) = &b.builtin {
        return builtin_lookup(builtin, package, env);
    }
    if !b.glob.is_empty() {
        // The name goes into a path: refuse anything that could leave the
        // intended folder or act as a wildcard.
        if package.contains(['/', '*']) || package.starts_with('.') {
            return None;
        }
        return Some(
            b.glob
                .iter()
                .flat_map(|g| env.glob(&g.replace("{package}", package)))
                .filter(|p| p.exists())
                .filter_map(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
                .collect(),
        );
    }
    let command: Vec<String> = b.command.as_ref()?.iter().map(|a| a.replace("{package}", package)).collect();
    let re = Regex::new(b.pattern.as_ref()?).ok()?;
    let out = process::run_ok(&command, TIMEOUT).ok()?;
    Some(
        out.lines()
            .filter_map(|line| re.captures(line))
            .filter_map(|caps| {
                if let Some(name) = caps.name("name") {
                    return Some(name.as_str().to_string());
                }
                let path = caps.name("path")?.as_str();
                Some(Path::new(path).file_name()?.to_string_lossy().into_owned())
            })
            .collect(),
    )
}

fn builtin_lookup(name: &str, package: &str, env: &Env) -> Option<Vec<String>> {
    match name {
        "cargo-bins" => cargo_bins(&fs::read_to_string(env.expand("~/.cargo/.crates2.json")?).ok()?, package),
        "npm-bins" => Some(npm_bins(env, package)),
        "pip-scripts" => Some(pip_scripts(env, package)),
        "pipx-apps" => {
            if package.contains('/') {
                return None;
            }
            let file = env.expand(&format!("~/.local/share/pipx/venvs/{package}/pipx_metadata.json"))?;
            pipx_apps(&fs::read_to_string(file).ok()?)
        }
        "uv-tools" => {
            if package.contains('/') {
                return None;
            }
            let file = env.expand(&format!("~/.local/share/uv/tools/{package}/uv-receipt.toml"))?;
            uv_entrypoints(&fs::read_to_string(file).ok()?)
        }
        _ => None,
    }
}

/// `~/.cargo/.crates2.json`: `{"installs": {"ripgrep 14.1.0 (registry+…)": {"bins": ["rg"]}}}`
fn cargo_bins(text: &str, package: &str) -> Option<Vec<String>> {
    let json: Value = serde_json::from_str(text).ok()?;
    let installs = json["installs"].as_object()?;
    Some(
        installs
            .iter()
            .filter(|(key, _)| key.split(' ').next() == Some(package))
            .flat_map(|(_, info)| info["bins"].as_array().into_iter().flatten())
            .filter_map(|b| b.as_str().map(String::from))
            .collect(),
    )
}

/// The `bin` field of the package's package.json, in every global
/// node_modules (all nvm Node versions).
fn npm_bins(env: &Env, package: &str) -> Vec<String> {
    let mut dirs = env.glob("~/.nvm/versions/node/*/lib/node_modules");
    dirs.extend(["/usr/lib/node_modules", "/usr/local/lib/node_modules"].map(Into::into));
    let mut names = Vec::new();
    for dir in dirs {
        let Ok(text) = fs::read_to_string(dir.join(package).join("package.json")) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match &json["bin"] {
            // "bin": "cli.js" means one command named after the package.
            Value::String(_) => names.push(package.rsplit('/').next().unwrap_or(package).to_string()),
            Value::Object(map) => names.extend(map.keys().cloned()),
            _ => {}
        }
    }
    names
}

/// Console and GUI scripts from the package's `entry_points.txt`, in the
/// folders the pip recipe lists from.
fn pip_scripts(env: &Env, package: &str) -> Vec<String> {
    let want = normalize_python(package);
    let mut names = Vec::new();
    for dir in pip_site_dirs(env) {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = file_name.strip_suffix(".dist-info").or_else(|| file_name.strip_suffix(".egg-info")) else {
                continue;
            };
            let dist = stem.split('-').next().unwrap_or(stem);
            if normalize_python(dist) != want {
                continue;
            }
            if let Ok(text) = fs::read_to_string(entry.path().join("entry_points.txt")) {
                names.extend(entry_point_scripts(&text));
            }
        }
    }
    names
}

/// The pip site folders pmm knows about (every Python version).
fn pip_site_dirs(env: &Env) -> Vec<std::path::PathBuf> {
    ["~/.local/lib/python*/site-packages", "/usr/local/lib/python*/dist-packages"]
        .iter()
        .flat_map(|g| env.glob(g))
        .collect()
}

/// Every command file pip installed, from the `RECORD` file pip writes for
/// each package (lines like `../../../bin/wal,sha256=…,123`). Scripts pip puts
/// in `~/.local/bin` sit in a folder no manager owns, so this is how they're
/// recognised as pip's.
pub fn pip_installed_commands(env: &Env) -> std::collections::HashSet<std::path::PathBuf> {
    let mut files = std::collections::HashSet::new();
    for site in pip_site_dirs(env) {
        for entry in fs::read_dir(&site).into_iter().flatten().flatten() {
            if !entry.file_name().to_string_lossy().ends_with(".dist-info") {
                continue;
            }
            let Ok(record) = fs::read_to_string(entry.path().join("RECORD")) else {
                continue;
            };
            for line in record.lines() {
                let rel = line.split(',').next().unwrap_or("");
                if rel.contains("/bin/") {
                    files.insert(normalize_lexically(&site.join(rel)));
                }
            }
        }
    }
    files
}

/// Resolve `..` and `.` without touching the filesystem.
pub fn normalize_lexically(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Python package names compare case-insensitively with `-`, `_` and `.`
/// all equal (PEP 503).
pub fn normalize_python(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// Script names in the `[console_scripts]` and `[gui_scripts]` sections.
fn entry_point_scripts(text: &str) -> Vec<String> {
    let mut in_scripts = false;
    let mut names = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_scripts = line == "[console_scripts]" || line == "[gui_scripts]";
        } else if in_scripts {
            if let Some((name, _)) = line.split_once('=') {
                names.push(name.trim().to_string());
            }
        }
    }
    names
}

/// pipx_metadata.json: `{"main_package": {"apps": ["wal"]}}`
fn pipx_apps(text: &str) -> Option<Vec<String>> {
    let json: Value = serde_json::from_str(text).ok()?;
    Some(
        json["main_package"]["apps"]
            .as_array()?
            .iter()
            .filter_map(|a| a.as_str().map(String::from))
            .collect(),
    )
}

/// uv-receipt.toml: `[tool] entrypoints = [{ name = "ruff", … }]`
fn uv_entrypoints(text: &str) -> Option<Vec<String>> {
    let receipt: toml::Table = toml::from_str(text).ok()?;
    Some(
        receipt
            .get("tool")?
            .get("entrypoints")?
            .as_array()?
            .iter()
            .filter_map(|e| e.get("name")?.as_str().map(String::from))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe;

    fn recipe_named(name: &str) -> Recipe {
        recipe::built_in().into_iter().find(|r| r.name == name).unwrap()
    }

    #[test]
    fn cargo_crates2() {
        let text = r#"{"installs": {
            "ripgrep 14.1.0 (registry+https://github.com/rust-lang/crates.io-index)": {"bins": ["rg"]},
            "fd-find 9.0.0 (registry+https://github.com/rust-lang/crates.io-index)": {"bins": ["fd"]}
        }}"#;
        assert_eq!(cargo_bins(text, "ripgrep"), Some(vec!["rg".into()]));
        assert_eq!(cargo_bins(text, "fd-find"), Some(vec!["fd".into()]));
        assert_eq!(cargo_bins(text, "bat"), Some(vec![]));
    }

    #[test]
    fn pipx_and_uv_files() {
        assert_eq!(pipx_apps(r#"{"main_package": {"apps": ["wal"]}}"#), Some(vec!["wal".into()]));
        let receipt = "[tool]\nrequirements = [{ name = \"ruff\" }]\nentrypoints = [\n  { name = \"ruff\", install-path = \"/h/.local/bin/ruff\" },\n]\n";
        assert_eq!(uv_entrypoints(receipt), Some(vec!["ruff".into()]));
    }

    #[test]
    fn python_names() {
        assert_eq!(normalize_python("Py_Wal"), "py-wal");
        assert_eq!(normalize_python("zope.interface"), "zope-interface");
        assert_eq!(normalize_python("a__-b"), "a-b");
    }

    #[test]
    fn entry_points() {
        let text = "[console_scripts]\nwal = pywal.__main__:main\n\n[gui_scripts]\nwal-gui = pywal.gui:main\n\n[pywal.plugins]\nnot_a_script = x:y\n";
        assert_eq!(entry_point_scripts(text), vec!["wal", "wal-gui"]);
    }

    #[test]
    fn pip_finds_dist_info_by_normalized_name() {
        let home = tempfile::tempdir().unwrap();
        let info = home.path().join(".local/lib/python3.12/site-packages/Py_Wal-3.3.0.dist-info");
        fs::create_dir_all(&info).unwrap();
        fs::write(info.join("entry_points.txt"), "[console_scripts]\nwal = pywal.__main__:main\n").unwrap();
        let env = Env { path_dirs: vec![], home: Some(home.path().into()) };
        assert_eq!(pip_scripts(&env, "py-wal"), vec!["wal"]);
    }

    #[test]
    fn pip_record_files_mark_commands_as_pips() {
        let home = tempfile::tempdir().unwrap();
        let site = home.path().join(".local/lib/python3.10/site-packages");
        let info = site.join("sherlock_project-0.15.0.dist-info");
        fs::create_dir_all(&info).unwrap();
        fs::write(
            info.join("RECORD"),
            "../../../bin/sherlock,sha256=abc,123\nsherlock/__init__.py,sha256=def,45\n",
        )
        .unwrap();
        let env = Env { path_dirs: vec![], home: Some(home.path().into()) };
        let files = pip_installed_commands(&env);
        assert!(files.contains(&home.path().join(".local/bin/sherlock")));
        // /usr/local is read too and belongs to the real machine; only count ours.
        assert_eq!(files.iter().filter(|f| f.starts_with(home.path())).count(), 1);
    }

    #[test]
    fn npm_bin_field_string_and_map() {
        let home = tempfile::tempdir().unwrap();
        let nm = home.path().join(".nvm/versions/node/v22.12.0/lib/node_modules");
        fs::create_dir_all(nm.join("typescript")).unwrap();
        fs::write(nm.join("typescript/package.json"), r#"{"bin": {"tsc": "bin/tsc", "tsserver": "bin/tsserver"}}"#).unwrap();
        fs::create_dir_all(nm.join("@expo/ngrok")).unwrap();
        fs::write(nm.join("@expo/ngrok/package.json"), r#"{"bin": "bin/ngrok"}"#).unwrap();
        let env = Env { path_dirs: vec![], home: Some(home.path().into()) };
        let npm = recipe_named("npm");
        assert_eq!(binaries_of(&npm, "typescript", &env), Some(vec!["tsc".into(), "tsserver".into()]));
        assert_eq!(binaries_of(&npm, "@expo/ngrok", &env), Some(vec!["ngrok".into()]));
    }

    #[test]
    fn globs_refuse_path_tricks() {
        let env = Env { path_dirs: vec![], home: Some("/home/u".into()) };
        let snap = recipe_named("snap");
        assert_eq!(binaries_of(&snap, "../../etc", &env), None);
        assert_eq!(binaries_of(&snap, "*", &env), None);
    }

    #[test]
    fn apt_pattern_keeps_only_command_folders() {
        let apt = recipe_named("apt");
        let re = Regex::new(apt.binaries.unwrap().pattern.as_deref().unwrap()).unwrap();
        let names: Vec<&str> = ["/usr/bin/git", "/usr/share/doc/git", "/bin/ls", "/usr/sbin/sshd", "/usr/lib/git-core/git", "/usr/games/sl"]
            .into_iter()
            .filter_map(|l| re.captures(l).map(|c| c.name("name").unwrap().as_str()))
            .collect();
        assert_eq!(names, vec!["git", "ls", "sshd", "sl"]);
    }

    #[test]
    fn no_binaries_section_means_unknown() {
        let env = Env { path_dirs: vec![], home: None };
        assert_eq!(binaries_of(&recipe_named("rustup"), "stable", &env), None);
    }
}
