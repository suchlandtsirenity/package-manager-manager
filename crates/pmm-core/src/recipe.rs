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
    /// How to find the commands a package provides. Missing means pmm can't
    /// tell which copy of a duplicated package runs.
    #[serde(default)]
    pub binaries: Option<Binaries>,
    /// How to update everything it has installed. Missing means pmm can't.
    #[serde(default)]
    pub update: Option<Update>,
    /// Ways to install the manager itself; the first is the default.
    #[serde(default)]
    pub install: Vec<Install>,
    /// What to do, besides uninstalling the manager, when it's removed.
    #[serde(default)]
    pub remove: Option<Remove>,
    /// How to install one package (`pmm install`, `pmm restore`).
    #[serde(default)]
    pub install_pkg: Option<PackageAction>,
    #[serde(default)]
    pub remove_pkg: Option<PackageAction>,
    #[serde(default)]
    pub upgrade_pkg: Option<PackageAction>,
    /// How to search the manager's repository for packages.
    #[serde(default)]
    pub search_remote: Option<SearchRemote>,
    /// How to show details about one package.
    #[serde(default)]
    pub info: Option<Info>,
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
    /// `version`, `title` (searchable display name) and `note` (optional).
    /// Lines that don't match are skipped.
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

/// How to find the commands one package provides. `{package}` in `glob` or
/// `command` is replaced by the package name. Use one of: `glob` (files whose
/// names are the commands), `command` + `pattern` (a `name` group, or a
/// `path` group whose file name is used), or `builtin`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Binaries {
    #[serde(default)]
    pub glob: Vec<String>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub builtin: Option<String>,
}

/// How to update everything a manager has installed.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Update {
    /// Commands run in order; pmm stops at the first one that fails.
    pub commands: Vec<Vec<String>>,
    /// The commands need root. pmm adds `sudo` itself (`pkexec` in the GUI),
    /// so don't put it in `commands`.
    #[serde(default)]
    pub sudo: bool,
}

/// One way to install a manager, e.g. with its official script or through
/// another manager.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Install {
    /// What `pmm add --via` calls it: the other manager's name, or e.g.
    /// "official" for the project's own installer.
    pub name: String,
    /// The manager this method installs through, which must be installed
    /// first. Also how pmm matches an installed manager to the method that
    /// put it there (detection says "go via brew"); a method without one
    /// matches managers that were installed standalone.
    #[serde(default)]
    pub manager: Option<String>,
    pub commands: Vec<Vec<String>>,
    #[serde(default)]
    pub sudo: bool,
    /// Commands that undo this method. Missing means pmm can't remove a
    /// manager installed this way; `[]` means there's nothing to run besides
    /// the recipe's `[remove]`.
    #[serde(default)]
    pub uninstall: Option<Vec<Vec<String>>>,
}

/// Removing a manager: first `commands` (remove its packages, if
/// uninstalling the manager doesn't), then the install method's
/// `uninstall`, then `paths` are deleted.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Remove {
    #[serde(default)]
    pub commands: Vec<Vec<String>>,
    #[serde(default)]
    pub sudo: bool,
    /// Files or folders it leaves behind, inside the home folder only
    /// (`~/...`; no `*` or `..`).
    #[serde(default)]
    pub paths: Vec<String>,
    /// Shown with the plan: cleanup pmm leaves to you, like lines an
    /// installer added to `~/.bashrc`.
    #[serde(default)]
    pub note: Option<String>,
}

/// A command acting on one package; `{package}` is replaced by its name and
/// `{version}` by its version (for managers like nvm, whose packages are all
/// called "node").
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PackageAction {
    pub command: Vec<String>,
    #[serde(default)]
    pub sudo: bool,
    /// A dry run (never as root) whose output lists every package the
    /// command would affect, matched by `preview_pattern`'s `name` group.
    /// For commands that ask before doing more than asked (apt removing
    /// dependents): without a terminal to ask in (the GUI), pmm shows this
    /// list in its own confirmation instead.
    #[serde(default)]
    pub preview: Option<Vec<String>>,
    #[serde(default)]
    pub preview_pattern: Option<String>,
    /// Added to `command` when nobody can answer its questions (the GUI,
    /// after its own confirmation), e.g. `["-y"]`.
    #[serde(default)]
    pub unattended: Vec<String>,
}

/// Searching a manager's repository. `{query}` in `command` is replaced by
/// what's searched for; each output line is matched against `pattern`, with
/// groups `name` (required), `version`, `title` and `description`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SearchRemote {
    pub command: Vec<String>,
    pub pattern: String,
    /// The repository can only look up exact names (PyPI has no search).
    #[serde(default)]
    pub exact: bool,
}

/// Details about one package: `command`'s output (stdout) is shown as is.
/// `{package}` is replaced by the name.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Info {
    pub command: Vec<String>,
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
    if let Some(binaries) = &recipe.binaries {
        check_binaries(binaries)?;
    }
    if let Some(update) = &recipe.update {
        if update.commands.is_empty() {
            bail!("[update] commands is empty");
        }
        check_commands("[update]", &update.commands)?;
    }
    check_install(&recipe.install)?;
    if let Some(remove) = &recipe.remove {
        check_commands("[remove]", &remove.commands)?;
        for path in &remove.paths {
            check_home_path(path)?;
        }
    }
    for (section, action) in [
        ("[install_pkg]", &recipe.install_pkg),
        ("[remove_pkg]", &recipe.remove_pkg),
        ("[upgrade_pkg]", &recipe.upgrade_pkg),
    ] {
        if let Some(action) = action {
            check_placeholder(section, &action.command, &["{package}", "{version}"])?;
            match (&action.preview, &action.preview_pattern) {
                (None, None) => {}
                (Some(preview), Some(pattern)) => {
                    check_placeholder(section, preview, &["{package}", "{version}"])?;
                    let re = Regex::new(pattern).with_context(|| format!("{section} preview_pattern isn't a valid regex"))?;
                    if !re.capture_names().flatten().any(|n| n == "name") {
                        bail!("{section} preview_pattern needs a (?P<name>...) group");
                    }
                }
                _ => bail!("{section} needs both `preview` and `preview_pattern`, or neither"),
            }
        }
    }
    if let Some(search) = &recipe.search_remote {
        check_placeholder("[search_remote]", &search.command, &["{query}"])?;
        let re = Regex::new(&search.pattern).context("[search_remote] pattern isn't a valid regex")?;
        if !re.capture_names().flatten().any(|n| n == "name") {
            bail!("[search_remote] pattern needs a (?P<name>...) group");
        }
    }
    if let Some(info) = &recipe.info {
        check_placeholder("[info]", &info.command, &["{package}"])?;
    }
    Ok(recipe)
}

fn check_commands(section: &str, commands: &[Vec<String>]) -> Result<()> {
    for command in commands {
        match command.first().map(String::as_str) {
            None => bail!("{section} has an empty command"),
            Some("sudo" | "pkexec" | "doas") => {
                bail!("{section} set `sudo = true` instead of starting a command with {}", command[0])
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// A command acting on something must say where its name goes.
fn check_placeholder(section: &str, command: &[String], placeholders: &[&str]) -> Result<()> {
    check_commands(section, std::slice::from_ref(&command.to_vec()))?;
    if !command.iter().any(|a| placeholders.iter().any(|p| a.contains(p))) {
        bail!("{section} command needs {}", placeholders.join(" or "));
    }
    Ok(())
}

fn check_install(methods: &[Install]) -> Result<()> {
    for (i, method) in methods.iter().enumerate() {
        let section = format!("[[install]] {:?}", method.name);
        if method.name.is_empty() {
            bail!("[[install]] needs a name");
        }
        if methods[..i].iter().any(|m| m.name == method.name) {
            bail!("{section} is listed twice");
        }
        if method.commands.is_empty() {
            bail!("{section} commands is empty");
        }
        check_commands(&section, &method.commands)?;
        if let Some(uninstall) = &method.uninstall {
            check_commands(&section, uninstall)?;
        }
    }
    Ok(())
}

/// `[remove] paths` are deleted without asking twice, so they're limited to
/// things that can only ever mean a folder inside home.
fn check_home_path(path: &str) -> Result<()> {
    let rest = path.strip_prefix("~/").unwrap_or("");
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|p| *p == ".." || *p == "." || p.contains('*')) {
        bail!("[remove] path {path:?} must be inside the home folder (\"~/...\", no `*`, `.` or `..`)");
    }
    Ok(())
}

fn check_binaries(b: &Binaries) -> Result<()> {
    match (b.glob.is_empty(), &b.command, &b.pattern, &b.builtin) {
        (false, None, None, None) => {}
        (true, Some(command), Some(pattern), None) => {
            if command.is_empty() {
                bail!("[binaries] command is empty");
            }
            let re = Regex::new(pattern).context("[binaries] pattern isn't a valid regex")?;
            if !re.capture_names().flatten().any(|n| n == "name" || n == "path") {
                bail!("[binaries] pattern needs a (?P<name>...) or (?P<path>...) group");
            }
        }
        (true, None, None, Some(name)) => {
            if !crate::binaries::BUILTINS.contains(&name.as_str()) {
                bail!(
                    "[binaries] builtin {name:?} doesn't exist (known: {})",
                    crate::binaries::BUILTINS.join(", ")
                );
            }
        }
        _ => bail!("[binaries] needs exactly one of `glob`, `command` and `pattern`, or `builtin`"),
    }
    Ok(())
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
    fn update_section_is_checked() {
        let base = "name = \"x\"\ndescription = \"x\"\n[detect]\nbinary = \"x\"\n[update]\n";
        assert!(parse(&format!("{base}commands = [[\"x\", \"up\"]]\nsudo = true\n")).is_ok());
        assert!(parse(&format!("{base}commands = []\n")).is_err());
        assert!(parse(&format!("{base}commands = [[]]\n")).is_err());
        let sudo = parse(&format!("{base}commands = [[\"sudo\", \"x\"]]\n")).unwrap_err();
        assert!(sudo.to_string().contains("sudo = true"), "{sudo}");
    }

    #[test]
    fn install_and_remove_are_checked() {
        let base = "name = \"x\"\ndescription = \"x\"\n[detect]\nbinary = \"x\"\n";
        let ok = format!(
            "{base}[[install]]\nname = \"brew\"\nmanager = \"brew\"\ncommands = [[\"brew\", \"install\", \"x\"]]\nuninstall = []\n\
             [remove]\npaths = [\"~/.x\", \"~/.local/share/x\"]\n\
             [install_pkg]\ncommand = [\"x\", \"add\", \"{{package}}\"]\n"
        );
        let recipe = parse(&ok).unwrap();
        assert_eq!(recipe.install[0].uninstall, Some(vec![]));

        let twice = format!("{base}[[install]]\nname = \"a\"\ncommands = [[\"a\"]]\n[[install]]\nname = \"a\"\ncommands = [[\"b\"]]\n");
        assert!(parse(&twice).unwrap_err().to_string().contains("twice"));
        for bad in ["/usr", "~", "~/", "~/../x", "~/a/*/b", "~/./x", ".x"] {
            let text = format!("{base}[remove]\npaths = [\"{bad}\"]\n");
            assert!(parse(&text).is_err(), "{bad} should be refused");
        }
        let no_placeholder = format!("{base}[install_pkg]\ncommand = [\"x\", \"add\"]\n");
        assert!(parse(&no_placeholder).is_err());
        let search = format!("{base}[search_remote]\ncommand = [\"x\", \"find\", \"{{query}}\"]\npattern = '^(?P<name>\\S+)'\n");
        assert!(parse(&search).is_ok());
        let search_no_name = format!("{base}[search_remote]\ncommand = [\"x\", \"{{query}}\"]\npattern = '^(\\S+)'\n");
        assert!(parse(&search_no_name).is_err());
        let info_no_placeholder = format!("{base}[info]\ncommand = [\"x\", \"info\"]\n");
        assert!(parse(&info_no_placeholder).is_err());
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
