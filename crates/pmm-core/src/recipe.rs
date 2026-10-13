//! Recipes: TOML files describing how pmm deals with one package manager.
//!
//! Built-in recipes are compiled into the binary. User recipes in
//! `~/.config/pmm/managers/*.toml` add new managers, or replace a built-in
//! one with the same `name`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub homepage: Option<String>,
    /// Part of the OS; pmm will never remove it.
    #[serde(default)]
    pub protected: bool,
    pub detect: Detect,
    /// How to list installed packages. Missing means pmm can't list them.
    #[serde(default)]
    pub list: Option<List>,
    /// Where this recipe came from. Filled in by the loader.
    #[serde(skip)]
    pub source: Source,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Detect {
    /// Executable looked up in PATH.
    #[serde(default)]
    pub binary: Option<String>,
    /// File whose existence means the manager is installed, for managers
    /// that aren't a binary (nvm is a shell function). `~` is expanded.
    #[serde(default)]
    pub path: Option<String>,
    /// Command whose output contains the manager's version.
    #[serde(default)]
    pub version: Option<Vec<String>>,
    /// Folders this manager installs things into. Used to work out which
    /// manager installed another one. `~` is expanded; `*` matches within
    /// one path component.
    #[serde(default)]
    pub owns: Vec<String>,
}

/// How to list what a manager has installed: either a command whose output
/// is matched line by line against `pattern`, or a reader built into pmm.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct List {
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Regex applied to each output line. Named groups: `name` (required),
    /// `version`, `title` (searchable display name) and `note` (optional). Lines that don't match are skipped.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Name of a reader built into pmm, for managers a pattern can't handle.
    #[serde(default)]
    pub builtin: Option<String>,
    /// What the list holds, e.g. "toolchains". Defaults to "packages".
    #[serde(default)]
    pub items: Option<String>,
    /// Files or folders that change when packages are installed or removed.
    /// The cached list is reused until one of them changes. `~` and `*` work.
    #[serde(default)]
    pub watch: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Source {
    #[default]
    BuiltIn,
    /// A user recipe; `overrides` is true when it replaced a built-in.
    User { path: PathBuf, overrides: bool },
}

const BUILT_IN: &[(&str, &str)] = &[
    ("apt", include_str!("../recipes/apt.toml")),
    ("brew", include_str!("../recipes/brew.toml")),
    ("cargo", include_str!("../recipes/cargo.toml")),
    ("flatpak", include_str!("../recipes/flatpak.toml")),
    ("go", include_str!("../recipes/go.toml")),
    ("nix", include_str!("../recipes/nix.toml")),
    ("npm", include_str!("../recipes/npm.toml")),
    ("nvm", include_str!("../recipes/nvm.toml")),
    ("pip", include_str!("../recipes/pip.toml")),
    ("pipx", include_str!("../recipes/pipx.toml")),
    ("pnpm", include_str!("../recipes/pnpm.toml")),
    ("rustup", include_str!("../recipes/rustup.toml")),
    ("snap", include_str!("../recipes/snap.toml")),
    ("uv", include_str!("../recipes/uv.toml")),
];

pub fn parse(text: &str) -> Result<Recipe> {
    let recipe: Recipe = toml::from_str(text)?;
    if recipe.detect.binary.is_none() && recipe.detect.path.is_none() {
        bail!("[detect] needs `binary` or `path`");
    }
    if let Some(list) = &recipe.list {
        check_list(list)?;
    }
    Ok(recipe)
}

fn check_list(list: &List) -> Result<()> {
    match (&list.builtin, &list.command, &list.pattern) {
        (Some(name), None, None) => {
            if !crate::inventory::BUILTINS.contains(&name.as_str()) {
                bail!(
                    "[list] builtin {name:?} doesn't exist (known: {})",
                    crate::inventory::BUILTINS.join(", ")
                );
            }
        }
        (None, Some(command), Some(pattern)) => {
            if command.is_empty() {
                bail!("[list] command is empty");
            }
            let re = Regex::new(pattern).context("[list] pattern isn't a valid regex")?;
            if !re.capture_names().flatten().any(|n| n == "name") {
                bail!("[list] pattern needs a (?P<name>...) group");
            }
        }
        _ => bail!("[list] needs either `builtin`, or `command` and `pattern`"),
    }
    Ok(())
}

pub fn built_in() -> Vec<Recipe> {
    BUILT_IN
        .iter()
        .map(|(name, text)| {
            parse(text).unwrap_or_else(|e| panic!("built-in recipe {name} is invalid: {e:#}"))
        })
        .collect()
}

/// Recipes found by [`load`], plus problems with user recipes. A broken user
/// recipe is skipped rather than stopping pmm.
#[derive(Debug, Default)]
pub struct Loaded {
    pub recipes: Vec<Recipe>,
    pub warnings: Vec<String>,
}

/// Built-in recipes merged with the user's, sorted by name.
pub fn load(user_dir: Option<&Path>) -> Loaded {
    let mut loaded = Loaded {
        recipes: built_in(),
        warnings: Vec::new(),
    };
    if let Some(dir) = user_dir {
        if let Err(e) = merge_user_dir(dir, &mut loaded) {
            loaded.warnings.push(format!("{}: {e:#}", dir.display()));
        }
    }
    loaded.recipes.sort_by(|a, b| a.name.cmp(&b.name));
    loaded
}

fn merge_user_dir(dir: &Path, loaded: &mut Loaded) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .context("can't read folder")?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();

    for path in paths {
        let result = fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| parse(&text));
        let mut recipe = match result {
            Ok(r) => r,
            Err(e) => {
                loaded.warnings.push(format!("{}: {e:#}", path.display()));
                continue;
            }
        };
        let existing = loaded.recipes.iter().position(|r| r.name == recipe.name);
        recipe.source = Source::User {
            path,
            overrides: existing.is_some(),
        };
        match existing {
            Some(i) => loaded.recipes[i] = recipe,
            None => loaded.recipes.push(recipe),
        }
    }
    Ok(())
}

/// `~/.config/pmm/managers`, honouring `XDG_CONFIG_HOME`.
pub fn default_user_dir() -> Option<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home_dir()?.join(".config"),
    };
    Some(config.join("pmm").join("managers"))
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_recipe_parses() {
        let recipes = built_in();
        assert_eq!(recipes.len(), BUILT_IN.len());
        for (file_name, _) in BUILT_IN {
            assert!(recipes.iter().any(|r| r.name == *file_name));
        }
    }

    #[test]
    fn recipe_needs_binary_or_path() {
        let err = parse("name = \"x\"\ndescription = \"x\"\n[detect]\n").unwrap_err();
        assert!(err.to_string().contains("binary"));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let text = "name = \"x\"\ndescription = \"x\"\nbinry = \"x\"\n[detect]\nbinary = \"x\"\n";
        assert!(parse(text).is_err());
    }

    #[test]
    fn list_section_is_checked() {
        let base = "name = \"x\"\ndescription = \"x\"\n[detect]\nbinary = \"x\"\n[list]\n";
        let ok = format!("{base}command = [\"x\"]\npattern = '^(?P<name>\\S+)'\n");
        assert!(parse(&ok).is_ok());
        let no_name = format!("{base}command = [\"x\"]\npattern = '^(\\S+)'\n");
        assert!(parse(&no_name).unwrap_err().to_string().contains("name"));
        let bad_builtin = format!("{base}builtin = \"nope\"\n");
        assert!(parse(&bad_builtin).unwrap_err().to_string().contains("doesn't exist"));
        let both = format!("{base}builtin = \"nvm\"\ncommand = [\"x\"]\n");
        assert!(parse(&both).is_err());
    }

    #[test]
    fn user_recipes_add_and_override() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("bun.toml"),
            "name = \"bun\"\ndescription = \"Bun\"\n[detect]\nbinary = \"bun\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("my-brew.toml"),
            "name = \"brew\"\ndescription = \"Mine\"\n[detect]\nbinary = \"brew\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("broken.toml"), "name = ").unwrap();
        fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let loaded = load(Some(dir.path()));

        let bun = loaded.recipes.iter().find(|r| r.name == "bun").unwrap();
        assert!(matches!(bun.source, Source::User { overrides: false, .. }));
        let brew = loaded.recipes.iter().find(|r| r.name == "brew").unwrap();
        assert_eq!(brew.description, "Mine");
        assert!(matches!(brew.source, Source::User { overrides: true, .. }));
        assert_eq!(loaded.recipes.iter().filter(|r| r.name == "brew").count(), 1);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("broken.toml"));
    }

    #[test]
    fn missing_user_dir_is_fine() {
        let loaded = load(Some(Path::new("/nonexistent/pmm/managers")));
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.recipes.len(), BUILT_IN.len());
    }
}
