//! Package readers for managers a line pattern can't handle. Each is named
//! in [`crate::inventory::BUILTINS`] and chosen with `[list] builtin = "..."`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::inventory::Package;
use crate::paths::Env;
use crate::process;

pub fn run(name: &str, env: &Env, timeout: Duration) -> Result<Vec<Package>> {
    match name {
        "npm-globals" => npm_globals(env, timeout),
        "pnpm-globals" => pnpm_globals(timeout),
        "nvm" => nvm(env),
        "go-bins" => go_bins(timeout),
        "nix" => nix(env, timeout),
        _ => bail!("no built-in reader called {name:?}"),
    }
}

fn cmd(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

// ---- npm ----------------------------------------------------------------

/// Packages that come with Node itself rather than `npm install -g`.
const BUNDLED_WITH_NODE: &[&str] = &["npm", "corepack"];

/// Global packages for every Node version under nvm. Without nvm, falls
/// back to whatever `npm root -g` reports.
fn npm_globals(env: &Env, timeout: Duration) -> Result<Vec<Package>> {
    let mut dirs = env.glob("~/.nvm/versions/node/*/lib/node_modules");
    if dirs.is_empty() {
        let root = process::run_ok(&cmd(&["npm", "root", "-g"]), timeout)?;
        dirs.push(PathBuf::from(root.trim()));
    }
    let mut packages = Vec::new();
    for dir in dirs {
        let note = node_version_of(&dir).map(|v| format!("node {v}"));
        packages.extend(read_node_modules(&dir, note.as_deref())?);
    }
    Ok(packages)
}

/// `…/node/v22.12.0/lib/node_modules` → `v22.12.0`.
fn node_version_of(node_modules: &Path) -> Option<String> {
    let version_dir = node_modules.parent()?.parent()?;
    let name = version_dir.file_name()?.to_str()?;
    name.starts_with('v').then(|| name.to_string())
}

fn read_node_modules(dir: &Path, note: Option<&str>) -> Result<Vec<Package>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut packages = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || BUNDLED_WITH_NODE.contains(&name.as_str()) {
            continue;
        }
        if name.starts_with('@') {
            for scoped in fs::read_dir(entry.path()).into_iter().flatten().flatten() {
                let full = format!("{name}/{}", scoped.file_name().to_string_lossy());
                packages.push(node_package(full, &scoped.path(), note));
            }
        } else {
            packages.push(node_package(name, &entry.path(), note));
        }
    }
    Ok(packages)
}

fn node_package(name: String, dir: &Path, note: Option<&str>) -> Package {
    let version = fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|json| json["version"].as_str().map(String::from));
    Package {
        title: None,
        name,
        version,
        note: note.map(String::from),
    }
}

// ---- pnpm ---------------------------------------------------------------

fn pnpm_globals(timeout: Duration) -> Result<Vec<Package>> {
    let out = process::run_ok(&cmd(&["pnpm", "ls", "-g", "--depth=0", "--json"]), timeout)?;
    parse_pnpm_json(&out)
}

/// `[{"path": …, "dependencies": {"name": {"version": "1.0.0"}}}]`
fn parse_pnpm_json(text: &str) -> Result<Vec<Package>> {
    let json: Value = serde_json::from_str(text).context("pnpm printed something that isn't JSON")?;
    let mut packages = Vec::new();
    for project in json.as_array().into_iter().flatten() {
        for (name, info) in project["dependencies"].as_object().into_iter().flatten() {
            packages.push(Package {
                title: None,
                name: name.clone(),
                version: info["version"].as_str().map(String::from),
                note: None,
            });
        }
    }
    Ok(packages)
}

// ---- nvm ----------------------------------------------------------------

/// Installed Node versions; the one first in PATH is marked active.
fn nvm(env: &Env) -> Result<Vec<Package>> {
    let active = env.find_in_path("node").and_then(|p| p.canonicalize().ok());
    Ok(env
        .glob("~/.nvm/versions/node/v*")
        .into_iter()
        .filter_map(|dir| {
            let version = dir.file_name()?.to_str()?.trim_start_matches('v').to_string();
            let is_active = active.as_ref().is_some_and(|a| a.starts_with(&dir));
            Some(Package {
                title: None,
                name: "node".into(),
                version: Some(version),
                note: is_active.then(|| "active".into()),
            })
        })
        .collect())
}

// ---- go -----------------------------------------------------------------

/// Binaries in GOBIN (or GOPATH/bin) that `go install` put there.
fn go_bins(timeout: Duration) -> Result<Vec<Package>> {
    let out = process::run_ok(&cmd(&["go", "env", "GOBIN", "GOPATH"]), timeout)?;
    let mut lines = out.lines();
    let gobin = lines.next().unwrap_or("").trim();
    let gopath = lines.next().unwrap_or("").trim();
    let dir = if !gobin.is_empty() {
        PathBuf::from(gobin)
    } else {
        // GOPATH can be a list; `go install` uses the first entry.
        match std::env::split_paths(gopath).next() {
            Some(first) if !first.as_os_str().is_empty() => first.join("bin"),
            _ => return Ok(Vec::new()),
        }
    };
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let dir = dir.to_string_lossy().into_owned();
    let out = process::run(&cmd(&["go", "version", "-m", &dir]), timeout)?;
    Ok(parse_go_version_m(&out.stdout))
}

/// Output of `go version -m <dir>`:
/// ```text
/// /home/u/go/bin/gopls: go1.22.0
///     path    golang.org/x/tools/gopls
///     mod     golang.org/x/tools/gopls    v0.15.0    h1:…
/// ```
fn parse_go_version_m(text: &str) -> Vec<Package> {
    let mut packages: Vec<Package> = Vec::new();
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            if let Some((path, _)) = line.rsplit_once(": ") {
                let name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned());
                if let Some(name) = name {
                    packages.push(Package { name, version: None, title: None, note: None });
                }
            }
            continue;
        }
        let Some(current) = packages.last_mut() else { continue };
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            ["path", path, ..] => current.note = Some(path.to_string()),
            ["mod", _, version, ..] => current.version = Some(version.trim_start_matches('v').to_string()),
            _ => {}
        }
    }
    packages
}

// ---- nix ----------------------------------------------------------------

/// The user's profile: `nix profile` (manifest.json) if that's in use,
/// otherwise `nix-env`. Doesn't run nix-env when there's no profile, because
/// that creates one.
fn nix(env: &Env, timeout: Duration) -> Result<Vec<Package>> {
    let Some(profile) = env.expand("~/.nix-profile") else {
        return Ok(Vec::new());
    };
    let manifest = profile.join("manifest.json");
    if manifest.is_file() {
        return parse_nix_manifest(&fs::read_to_string(manifest)?);
    }
    if !profile.exists() {
        return Ok(Vec::new());
    }
    let out = process::run_ok(&cmd(&["nix-env", "-q", "--json"]), timeout)?;
    parse_nix_env_json(&out)
}

/// `nix profile` manifest. Version 3 keys elements by name; 1 and 2 use a list.
fn parse_nix_manifest(text: &str) -> Result<Vec<Package>> {
    let json: Value = serde_json::from_str(text).context("manifest.json isn't valid JSON")?;
    let elements: Vec<(Option<&str>, &Value)> = match &json["elements"] {
        Value::Object(map) => map.iter().map(|(k, v)| (Some(k.as_str()), v)).collect(),
        Value::Array(list) => list.iter().map(|v| (None, v)).collect(),
        _ => Vec::new(),
    };
    Ok(elements
        .into_iter()
        .filter_map(|(key, element)| {
            let store_path = element["storePaths"].get(0)?.as_str()?;
            let (name, version) = parse_store_path(store_path)?;
            Some(Package {
                title: None,
                name: key.map(String::from).unwrap_or(name),
                version,
                note: None,
            })
        })
        .collect())
}

/// `{"nixpkgs.hello": {"pname": "hello", "version": "2.12.1", …}}`
fn parse_nix_env_json(text: &str) -> Result<Vec<Package>> {
    let json: Value = serde_json::from_str(text).context("nix-env printed something that isn't JSON")?;
    Ok(json
        .as_object()
        .into_iter()
        .flatten()
        .map(|(attr, info)| Package {
            title: None,
            name: info["pname"].as_str().unwrap_or(attr).to_string(),
            version: info["version"].as_str().filter(|v| !v.is_empty()).map(String::from),
            note: None,
        })
        .collect())
}

/// `/nix/store/<hash>-hello-2.12.1` → ("hello", Some("2.12.1")). Like Nix's
/// own rule: the version starts at the first `-` followed by a digit.
fn parse_store_path(path: &str) -> Option<(String, Option<String>)> {
    let base = path.rsplit('/').next()?;
    let (_hash, rest) = base.split_once('-')?;
    let split = rest
        .char_indices()
        .find(|&(i, c)| c == '-' && rest[i + 1..].starts_with(|d: char| d.is_ascii_digit()))
        .map(|(i, _)| i);
    Some(match split {
        Some(i) => (rest[..i].to_string(), Some(rest[i + 1..].to_string())),
        None => (rest.to_string(), None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, version: Option<&str>, note: Option<&str>) -> Package {
        Package { name: name.into(), version: version.map(Into::into), title: None, note: note.map(Into::into) }
    }

    #[test]
    fn npm_reads_every_node_version_and_skips_bundled() {
        let home = tempfile::tempdir().unwrap();
        let write = |rel: &str, version: &str| {
            let dir = home.path().join(rel);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("package.json"), format!("{{\"version\": \"{version}\"}}")).unwrap();
        };
        write(".nvm/versions/node/v18.17.0/lib/node_modules/npm", "9.6.7");
        write(".nvm/versions/node/v18.17.0/lib/node_modules/corepack", "0.18.0");
        write(".nvm/versions/node/v22.12.0/lib/node_modules/typescript", "5.6.3");
        write(".nvm/versions/node/v22.12.0/lib/node_modules/@expo/cli", "0.22.0");
        fs::create_dir_all(home.path().join(".nvm/versions/node/v22.12.0/lib/node_modules/.bin")).unwrap();
        let env = Env { path_dirs: vec![], home: Some(home.path().into()) };

        let mut got = npm_globals(&env, Duration::from_secs(5)).unwrap();
        got.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            got,
            vec![
                pkg("@expo/cli", Some("0.22.0"), Some("node v22.12.0")),
                pkg("typescript", Some("5.6.3"), Some("node v22.12.0")),
            ]
        );
    }

    #[test]
    fn pnpm_json() {
        let text = r#"[{"path": "/x", "dependencies": {"vite": {"version": "5.4.0"}}}, {"path": "/y"}]"#;
        assert_eq!(parse_pnpm_json(text).unwrap(), vec![pkg("vite", Some("5.4.0"), None)]);
        assert_eq!(parse_pnpm_json(r#"[{"path": "/x", "private": false}]"#).unwrap(), vec![]);
    }

    #[test]
    fn go_version_m() {
        let text = "/home/u/go/bin/gopls: go1.22.0\n\
                    \tpath\tgolang.org/x/tools/gopls\n\
                    \tmod\tgolang.org/x/tools/gopls\tv0.15.0\th1:abc=\n\
                    \tdep\tgolang.org/x/mod\tv0.15.0\th1:def=\n\
                    /home/u/go/bin/local: go1.22.0\n\
                    \tpath\texample.com/local\n\
                    \tmod\texample.com/local\t(devel)\t\n";
        assert_eq!(
            parse_go_version_m(text),
            vec![
                pkg("gopls", Some("0.15.0"), Some("golang.org/x/tools/gopls")),
                pkg("local", Some("(devel)"), Some("example.com/local")),
            ]
        );
    }

    #[test]
    fn nix_store_paths() {
        let p = |s: &str| parse_store_path(s).unwrap();
        assert_eq!(p("/nix/store/abc123-hello-2.12.1"), ("hello".into(), Some("2.12.1".into())));
        assert_eq!(p("/nix/store/abc123-python3.12-requests-2.31.0"), ("python3.12-requests".into(), Some("2.31.0".into())));
        assert_eq!(p("/nix/store/abc123-source"), ("source".into(), None));
    }

    #[test]
    fn nix_manifests_v2_and_v3() {
        let v2 = r#"{"version": 2, "elements": [{"storePaths": ["/nix/store/h-ripgrep-14.1.0"]}]}"#;
        assert_eq!(parse_nix_manifest(v2).unwrap(), vec![pkg("ripgrep", Some("14.1.0"), None)]);
        let v3 = r#"{"version": 3, "elements": {"rg": {"storePaths": ["/nix/store/h-ripgrep-14.1.0"]}}}"#;
        assert_eq!(parse_nix_manifest(v3).unwrap(), vec![pkg("rg", Some("14.1.0"), None)]);
    }

    #[test]
    fn nix_env_json() {
        let text = r#"{"nixpkgs.hello": {"pname": "hello", "version": "2.12.1"}}"#;
        assert_eq!(parse_nix_env_json(text).unwrap(), vec![pkg("hello", Some("2.12.1"), None)]);
        assert_eq!(parse_nix_env_json("{}").unwrap(), vec![]);
    }

    #[test]
    fn nix_without_profile_runs_nothing() {
        let home = tempfile::tempdir().unwrap();
        let env = Env { path_dirs: vec![], home: Some(home.path().into()) };
        assert_eq!(nix(&env, Duration::from_secs(1)).unwrap(), vec![]);
    }
}
