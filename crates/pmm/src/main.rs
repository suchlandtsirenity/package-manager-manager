use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use clap::{Parser, Subcommand};
use owo_colors::{OwoColorize, Stream::Stdout};
use pmm_core::{
    Cache, Detection, Env, Inventory, Owners, Rank, Source, Status, Via, cache, detect_all,
    list_all, package_dupes, recipe, search, shadowed_commands, update, which_runs,
};
use pmm_core::action::{self, Event, Io, Step};
use pmm_core::manage::{self, Restore};
use pmm_core::update::Plan;
use pmm_core::dupes::dupe_key;
use pmm_core::{Recipe, manifest, remote};

/// pmm — the package manager manager.
#[derive(Parser)]
#[command(version, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show every package manager pmm knows about and whether it's installed.
    Managers,
    /// Count installed packages per manager, or list one manager's packages.
    List {
        /// Only this manager, with every package listed.
        #[arg(short, long)]
        manager: Option<String>,
        /// Re-read from the managers instead of using the cache.
        #[arg(long)]
        refresh: bool,
    },
    /// Find packages by name: what's installed first, then what each
    /// manager's repository has.
    Search {
        query: String,
        /// Only search this manager (and show all its results).
        #[arg(short, long)]
        manager: Option<String>,
        /// Only what's installed.
        #[arg(long, conflicts_with = "remote")]
        installed: bool,
        /// Only the repositories.
        #[arg(long)]
        remote: bool,
        /// Show every repository result, not just the best few.
        #[arg(short, long)]
        all: bool,
        /// Re-read installed packages instead of using the cache.
        #[arg(long)]
        refresh: bool,
    },
    /// Details about a package from each manager that knows it.
    Info {
        package: String,
        /// Only ask this manager (and show everything it says).
        #[arg(short, long)]
        manager: Option<String>,
    },
    /// Install a package. Without --manager, shows which managers have it
    /// and asks.
    Install {
        package: String,
        #[arg(short, long)]
        manager: Option<String>,
    },
    /// Uninstall a package. Asks which copy if more than one manager has it.
    Uninstall {
        package: String,
        #[arg(short, long)]
        manager: Option<String>,
        /// Don't ask for confirmation.
        #[arg(short, long)]
        yes: bool,
    },
    /// Upgrade one package. Asks which copy if more than one manager has it.
    Upgrade {
        package: String,
        #[arg(short, long)]
        manager: Option<String>,
    },
    /// Find things installed more than once, and which copy actually runs.
    Dupes {
        /// List every command instead of grouping them.
        #[arg(short, long)]
        all: bool,
        /// Re-read from the managers instead of using the cache.
        #[arg(long)]
        refresh: bool,
    },
    /// Overview: managers, package count, duplicates, last changes.
    Status {
        /// Re-read from the managers instead of using the cache.
        #[arg(long)]
        refresh: bool,
    },
    /// Show which installed packages have a newer version.
    Outdated {
        /// Only this manager.
        #[arg(short, long)]
        manager: Option<String>,
    },
    /// Update everything each manager has installed.
    Update {
        /// Only this manager.
        #[arg(short, long)]
        manager: Option<String>,
        /// Show what would run, without running it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Install a package manager (and any manager it's installed through).
    Add {
        manager: String,
        /// How to install it: another manager's name, or e.g. "official".
        #[arg(long)]
        via: Option<String>,
        /// Show what would run, without running it.
        #[arg(long)]
        dry_run: bool,
        /// Don't ask before installing other managers it needs.
        #[arg(short, long)]
        yes: bool,
    },
    /// Remove a package manager and everything it installed. A list of its
    /// packages is saved first, for `pmm restore`.
    Remove {
        manager: String,
        /// Show what would be removed, without removing anything.
        #[arg(long)]
        dry_run: bool,
        /// Don't ask for confirmation.
        #[arg(short, long)]
        yes: bool,
    },
    /// Reinstall the packages of a removed manager. Without a list, shows
    /// the saved ones.
    Restore {
        /// A saved list: its name (e.g. pipx-2026-10-13) or path.
        list: Option<String>,
        /// Install them with this manager instead of the one they came from.
        #[arg(long)]
        via: Option<String>,
        /// Show what would be installed, without installing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> ExitCode {
    // Exit quietly when output is piped into something that stops reading
    // (`pmm list -m apt | head`), like other Unix tools, instead of panicking.
    // SAFETY: called once at startup, before any threads exist.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let loaded = recipe::load(recipe::default_user_dir().as_deref());
    for warning in &loaded.warnings {
        eprintln!("warning: skipped recipe {warning}");
    }
    let env = Env::current();
    let cache = Cache::new(Cache::default_dir());

    let result = match cli.command {
        Cmd::Managers => {
            managers(&detect_all(&loaded.recipes, &env, true));
            Ok(())
        }
        Cmd::List { manager, refresh } => {
            let detections = select(detect_all(&loaded.recipes, &env, false), manager.as_deref());
            detections.and_then(|d| {
                let inventories = list_all(&d, &env, &cache, refresh);
                match manager {
                    Some(_) => list_one(&inventories),
                    None => {
                        list_counts(&inventories);
                        Ok(())
                    }
                }
            })
        }
        Cmd::Search { query, manager, installed, remote, all, refresh } => {
            select_installed(detect_all(&loaded.recipes, &env, false), manager.as_deref()).and_then(|d| {
                let inventories = list_all(&d, &env, &cache, refresh);
                let mut found = false;
                if !remote {
                    found |= search_installed(&inventories, &query);
                }
                if !installed {
                    found |= search_repositories(&d, &inventories, &query, all || manager.is_some(), !remote)?;
                }
                if found { Ok(()) } else { Err(Failure::NoMatches) }
            })
        }
        Cmd::Info { package, manager } => {
            select_installed(detect_all(&loaded.recipes, &env, false), manager.as_deref()).and_then(|d| {
                let inventories = list_all(&d, &env, &cache, false);
                info(&d, &inventories, &package, manager.is_some())
            })
        }
        Cmd::Install { package, manager } => {
            let detections = detect_all(&loaded.recipes, &env, false);
            install(&detections, &package, manager.as_deref(), &env, &cache)
        }
        Cmd::Uninstall { package, manager, yes } => {
            let detections = detect_all(&loaded.recipes, &env, false);
            uninstall(&detections, &package, manager.as_deref(), yes, &env, &cache)
        }
        Cmd::Upgrade { package, manager } => {
            let detections = detect_all(&loaded.recipes, &env, false);
            upgrade(&detections, &package, manager.as_deref(), &env, &cache)
        }
        Cmd::Dupes { all, refresh } => {
            let detections = detect_all(&loaded.recipes, &env, false);
            let inventories = list_all(&detections, &env, &cache, refresh);
            dupes(&detections, &inventories, &env, all);
            Ok(())
        }
        Cmd::Status { refresh } => {
            let detections = detect_all(&loaded.recipes, &env, false);
            let inventories = list_all(&detections, &env, &cache, refresh);
            status(&detections, &inventories, &env, &cache);
            Ok(())
        }
        Cmd::Outdated { manager } => {
            select_installed(detect_all(&loaded.recipes, &env, false), manager.as_deref()).and_then(|d| {
                let inventories = list_all(&d, &env, &cache, false);
                outdated(&d, &inventories, manager.as_deref())
            })
        }
        Cmd::Update { manager, dry_run } => {
            select_installed(detect_all(&loaded.recipes, &env, false), manager.as_deref()).and_then(|d| {
                let inventories = list_all(&d, &env, &cache, false);
                update_all(&d, &inventories, dry_run)
            })
        }
        Cmd::Add { manager, via, dry_run, yes } => {
            add(&manager, via.as_deref(), dry_run, yes, &loaded.recipes, &env)
        }
        Cmd::Remove { manager, dry_run, yes } => {
            remove(&manager, dry_run, yes, &detect_all(&loaded.recipes, &env, false), &env, &cache)
        }
        Cmd::Restore { list, via, dry_run } => {
            let detections = detect_all(&loaded.recipes, &env, false);
            match list {
                None => {
                    saved_lists(&env);
                    Ok(())
                }
                Some(list) => restore(&list, via.as_deref(), dry_run, &detections, &env, &cache),
            }
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::NoMatches) => ExitCode::from(1),
        Err(Failure::Message(msg)) => {
            eprintln!("error: {msg}");
            ExitCode::from(2)
        }
    }
}

enum Failure {
    /// Nothing found; exit 1 without a message, like grep.
    NoMatches,
    Message(String),
}

/// Keep only `manager` if one was named, checking that it exists, is
/// installed and can be listed.
fn select(detections: Vec<Detection>, manager: Option<&str>) -> Result<Vec<Detection>, Failure> {
    let selected = select_installed(detections, manager)?;
    if let (Some(name), Some(d)) = (manager, selected.first()) {
        if d.recipe.list.is_none() {
            return Err(Failure::Message(format!("pmm can't list {name}'s packages yet")));
        }
    }
    Ok(selected)
}

/// Keep only `manager` if one was named, checking that it exists and is installed.
fn select_installed(detections: Vec<Detection>, manager: Option<&str>) -> Result<Vec<Detection>, Failure> {
    let Some(name) = manager else {
        return Ok(detections);
    };
    let Some(d) = detections.iter().find(|d| d.recipe.name == name) else {
        let known: Vec<&str> = detections.iter().map(|d| d.recipe.name.as_str()).collect();
        return Err(Failure::Message(format!(
            "pmm doesn't know a manager called {name:?}. Known: {}",
            known.join(", ")
        )));
    };
    if !d.is_installed() {
        return Err(Failure::Message(format!("{name} isn't installed")));
    }
    Ok(vec![d.clone()])
}

fn managers(detections: &[Detection]) {
    let mut sorted: Vec<&Detection> = detections.iter().collect();
    sorted.sort_by_key(|d| (!d.is_installed(), d.recipe.name.clone()));

    let installed = sorted.iter().filter(|d| d.is_installed()).count();
    println!("Package managers: {installed} of {} installed\n", sorted.len());

    let name_width = sorted.iter().map(|d| d.recipe.name.len()).max().unwrap_or(0);
    for d in sorted {
        let (version, how) = match &d.status {
            Status::NotInstalled => (String::new(), "not installed".to_string()),
            Status::Installed { version, via, .. } => {
                let how = match via {
                    _ if d.recipe.protected => "system".to_string(),
                    Via::Manager(m) => format!("via {m}"),
                    Via::Standalone => "standalone".to_string(),
                };
                (version.clone().unwrap_or_else(|| "?".into()), how)
            }
        };
        let custom = match &d.recipe.source {
            Source::BuiltIn => "",
            Source::User { overrides: true, .. } => "  [your recipe, replaces built-in]",
            Source::User { overrides: false, .. } => "  [your recipe]",
        };
        let line = format!(
            "  {:<name_width$}  {version:<9} {how:<14} {}{custom}",
            d.recipe.name, d.recipe.description
        );
        if d.is_installed() {
            println!("{line}");
        } else {
            println!("{}", line.if_supports_color(Stdout, |l| l.dimmed()));
        }
    }
}

/// "1 package", "3 packages", "1 Node version".
fn count(n: usize, items: &str) -> String {
    match (n, items.strip_suffix('s')) {
        (1, Some(singular)) => format!("1 {singular}"),
        _ => format!("{n} {items}"),
    }
}

fn list_counts(inventories: &[Inventory]) {
    let total: usize = inventories
        .iter()
        .filter(|i| i.items == "packages")
        .filter_map(|i| i.packages.as_ref().ok())
        .map(Vec::len)
        .sum();
    println!("Installed: {} across {} managers\n", count(total, "packages"), inventories.len());

    let name_width = inventories.iter().map(|i| i.manager.len()).max().unwrap_or(0);
    let mut sorted: Vec<&Inventory> = inventories.iter().collect();
    sorted.sort_by(|a, b| a.manager.cmp(&b.manager));
    for inv in sorted {
        match &inv.packages {
            Ok(packages) => {
                let n = packages.len();
                let line = format!("  {:<name_width$}  {:>5} {}", inv.manager, n, count(n, &inv.items).split_once(' ').map_or("", |(_, w)| w));
                if n == 0 {
                    println!("{}", line.if_supports_color(Stdout, |l| l.dimmed()));
                } else {
                    println!("{line}");
                }
            }
            Err(e) => println!(
                "  {:<name_width$}      {}",
                inv.manager,
                format!("couldn't read: {e}").if_supports_color(Stdout, |l| l.red())
            ),
        }
    }
    if inventories.iter().any(|i| i.from_cache) {
        println!(
            "\n{}",
            "Some counts are cached. `pmm list --refresh` re-reads everything."
                .if_supports_color(Stdout, |l| l.dimmed())
        );
    }
}

fn list_one(inventories: &[Inventory]) -> Result<(), Failure> {
    let inv = inventories.first().ok_or(Failure::NoMatches)?;
    let packages = inv.packages.as_ref().map_err(|e| Failure::Message(format!("couldn't read {}: {e}", inv.manager)))?;
    println!("{}: {}\n", inv.manager, count(packages.len(), &inv.items));
    let name_width = packages.iter().map(|p| p.name.len()).max().unwrap_or(0);
    let version_width = packages.iter().filter_map(|p| p.version.as_ref()).map(String::len).max().unwrap_or(0);
    for p in packages {
        let version = p.version.as_deref().unwrap_or("");
        let line = format!("  {:<name_width$}  {:<version_width$}  {}", p.name, version, extra(p));
        println!("{}", line.trim_end());
    }
    Ok(())
}

/// Title and note for display: "GitButler", "node v22.12.0", or both.
fn extra(p: &pmm_core::Package) -> String {
    [p.title.as_deref(), p.note.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Print installed matches; false if there were none.
fn search_installed(inventories: &[Inventory], query: &str) -> bool {
    for inv in inventories {
        if let Err(e) = &inv.packages {
            eprintln!("warning: couldn't read {}: {e}", inv.manager);
        }
    }
    let hits = search(inventories, query);
    if hits.is_empty() {
        println!("{}", format!("Nothing installed matches {query:?}.").if_supports_color(Stdout, |l| l.dimmed()));
        return false;
    }
    let n = hits.len();
    heading(&format!("Installed: {n} {} for {query:?}\n", if n == 1 { "match" } else { "matches" }));

    let manager_width = hits.iter().map(|h| h.manager.len()).max().unwrap_or(0);
    let name_width = hits.iter().map(|h| h.package.name.len()).max().unwrap_or(0);
    let version_width = hits.iter().filter_map(|h| h.package.version.as_ref()).map(String::len).max().unwrap_or(0);
    for hit in &hits {
        let p = hit.package;
        let name = format!("{:<name_width$}", p.name);
        let name = if hit.rank == Rank::Exact {
            name.if_supports_color(Stdout, |n| n.bold()).to_string()
        } else {
            name
        };
        let version = format!("{:<version_width$}", p.version.as_deref().unwrap_or(""));
        let line = format!(
            "  {}  {name}  {version}  {}",
            format!("{:<manager_width$}", hit.manager).if_supports_color(Stdout, |m| m.cyan()),
            extra(p).if_supports_color(Stdout, |n| n.dimmed())
        );
        println!("{}", line.trim_end());
    }

    // The same exact name under more than one manager is probably the same
    // tool installed twice. (Which copy runs is `pmm dupes`, coming next.)
    let mut exact: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for hit in hits.iter().filter(|h| h.rank == Rank::Exact) {
        let managers = exact.entry(hit.package.name.as_str()).or_default();
        if !managers.contains(&hit.manager) {
            managers.push(hit.manager);
        }
    }
    for (name, managers) in exact.iter().filter(|(_, m)| m.len() > 1) {
        println!(
            "\n{}",
            format!("⚠ {name} is installed via {} managers: {}", managers.len(), managers.join(", "))
                .if_supports_color(Stdout, |w| w.yellow())
        );
    }
    true
}

/// A path with the home folder shown as `~`.
fn tilde(path: &Path, env: &Env) -> String {
    match env.home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn owner_label(owner: &Option<String>) -> &str {
    owner.as_deref().unwrap_or("no manager")
}

fn heading(text: &str) {
    println!("{}", text.if_supports_color(Stdout, |t| t.bold()));
}

/// Who owns a copy, with the folder for files no manager owns, since that's
/// the only clue to where they came from.
fn copy_label(copy: &pmm_core::dupes::Copy, env: &Env) -> String {
    match &copy.owner {
        Some(manager) => manager.clone(),
        None => match copy.location.parent() {
            Some(dir) => format!("no manager ({})", tilde(dir, env)),
            None => "no manager".into(),
        },
    }
}

/// "a, b, c, d, +5 more" — or every name with `all`.
fn some_names(names: &[&str], all: bool) -> String {
    const SHOWN: usize = 4;
    if all || names.len() <= SHOWN + 1 {
        names.join(", ")
    } else {
        format!("{}, +{} more", names[..SHOWN].join(", "), names.len() - SHOWN)
    }
}

/// Group items by a key, keeping first-seen order of keys and items.
fn group_by<K: PartialEq, V>(items: impl IntoIterator<Item = (K, V)>) -> Vec<(K, Vec<V>)> {
    let mut groups: Vec<(K, Vec<V>)> = Vec::new();
    for (key, value) in items {
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, values)) => values.push(value),
            None => groups.push((key, vec![value])),
        }
    }
    groups
}

fn dupes(detections: &[Detection], inventories: &[Inventory], env: &Env, all: bool) {
    let owners = Owners::new(detections, env);
    let packages = package_dupes(inventories);
    let shadowed = shadowed_commands(env, &owners);
    if packages.is_empty() && shadowed.is_empty() {
        println!("No duplicates found.");
        return;
    }

    if !packages.is_empty() {
        heading("Same package from more than one manager\n");
        let width = packages.iter().map(|d| d.copies[0].1.name.len()).max().unwrap_or(0);
        for dupe in &packages {
            let copies: Vec<String> = dupe
                .copies
                .iter()
                .map(|(m, p)| format!("{m} {}", p.version.as_deref().unwrap_or("")).trim_end().to_string())
                .collect();
            println!("  {:<width$}  {}", dupe.copies[0].1.name, copies.join(" · "));

            // "runs brew's: bash, bashbug · apt's: rbash"
            let commands = which_runs(dupe, detections, &owners, env);
            let by_owner = group_by(commands.iter().map(|c| {
                let owner = match &c.runs {
                    Some(copy) => format!("{}'s", copy_label(copy, env)),
                    None => "nothing (not on PATH)".to_string(),
                };
                (owner, c.name.as_str())
            }));
            if !by_owner.is_empty() {
                let parts: Vec<String> = by_owner
                    .iter()
                    .map(|(owner, names)| format!("{owner}: {}", some_names(names, all)))
                    .collect();
                let line = format!("runs {}", parts.join(" · "));
                println!("  {:<width$}  {}", "", line.if_supports_color(Stdout, |d| d.dimmed()));
            }
        }
    }

    if !shadowed.is_empty() {
        if !packages.is_empty() {
            println!();
        }
        heading("Same command in more than one place on PATH (the first one runs)\n");
        if !all {
            // One line per "X hides Y" combination instead of per command.
            let groups = group_by(shadowed.iter().map(|s| {
                let labels: Vec<String> = s.copies.iter().map(|c| copy_label(c, env)).collect();
                (labels, s.name.as_str())
            }));
            let rows: Vec<(String, &Vec<&str>)> = groups
                .iter()
                .map(|(labels, names)| (format!("{} hides {}", labels[0], labels[1..].join(", ")), names))
                .collect();
            let width = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
            for (label, names) in &rows {
                let n = count(names.len(), "commands");
                println!(
                    "  {label:<width$}  {n:<12}  {}",
                    some_names(names, false).if_supports_color(Stdout, |d| d.dimmed())
                );
            }
            println!(
                "\n{}",
                "`pmm dupes --all` lists every command and path.".if_supports_color(Stdout, |d| d.dimmed())
            );
            return;
        }
        let width = shadowed.iter().map(|s| s.name.len()).max().unwrap_or(0);
        let owner_width = shadowed
            .iter()
            .flat_map(|s| &s.copies)
            .map(|c| owner_label(&c.owner).len())
            .max()
            .unwrap_or(0);
        for s in &shadowed {
            for (i, copy) in s.copies.iter().enumerate() {
                let name = if i == 0 { s.name.as_str() } else { "" };
                let line = format!(
                    "  {name:<width$}  {:<owner_width$}  {}",
                    owner_label(&copy.owner),
                    tilde(&copy.location, env)
                );
                if i == 0 {
                    println!("{line}");
                } else {
                    println!("{}", line.if_supports_color(Stdout, |l| l.dimmed()));
                }
            }
        }
    }
}

/// "just now", "5 min ago", "3 hours ago", "12 days ago".
fn ago(when: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(when).map(|d| d.as_secs()).unwrap_or(0);
    let plural = |n: u64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => plural(secs / 3600, "hour"),
        86400..31_536_000 => plural(secs / 86400, "day"),
        _ => plural(secs / 31_536_000, "year"),
    }
}

fn status(detections: &[Detection], inventories: &[Inventory], env: &Env, cache: &Cache) {
    let installed = detections.iter().filter(|d| d.is_installed()).count();
    let packages: usize = inventories
        .iter()
        .filter(|i| i.items == "packages")
        .filter_map(|i| i.packages.as_ref().ok())
        .map(Vec::len)
        .sum();
    let owners = Owners::new(detections, env);
    let package_dupes = package_dupes(inventories).len();
    let shadowed = shadowed_commands(env, &owners).len();

    println!("Package managers  {installed} installed (of {} known)", detections.len());
    println!("Packages          {packages}");
    let dupes = if package_dupes + shadowed == 0 {
        "none".to_string()
    } else {
        format!(
            "{} in several managers · {} shadowed on PATH  (see `pmm dupes`)",
            count(package_dupes, "packages"),
            count(shadowed, "commands"),
        )
    };
    println!("Duplicates        {dupes}");
    let failed: Vec<&str> = inventories
        .iter()
        .filter(|i| i.packages.is_err())
        .map(|i| i.manager.as_str())
        .collect();
    if !failed.is_empty() {
        let line = format!("Couldn't read     {}", failed.join(", "));
        println!("{}", line.if_supports_color(Stdout, |l| l.red()));
    }

    // Newest first; managers whose watched files don't exist go last.
    let mut changes: Vec<(&str, Option<SystemTime>)> = detections
        .iter()
        .filter(|d| d.is_installed())
        .filter_map(|d| {
            let list = d.recipe.list.as_ref()?;
            Some((d.recipe.name.as_str(), cache::last_change(&cache.fingerprint(list, env))))
        })
        .collect();
    changes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    println!("\nLast change (install, removal or upgrade)");
    let width = changes.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, when) in changes {
        match when {
            Some(t) => println!("  {name:<width$}  {}", ago(t)),
            None => {
                let line = format!("  {name:<width$}  unknown");
                println!("{}", line.if_supports_color(Stdout, |l| l.dimmed()));
            }
        }
    }
}

/// How the CLI gets root for steps that need it.
const ELEVATE: &str = "sudo";

/// Ask for the sudo password once, up front, if any of `who` (the things
/// needing it) is non-empty, rather than in the middle of other output.
fn get_sudo(who: &[&str]) -> bool {
    if who.is_empty() {
        return true;
    }
    println!("\n{} {} sudo.", who.join(" and "), if who.len() == 1 { "needs" } else { "need" });
    std::process::Command::new(ELEVATE).arg("-v").status().is_ok_and(|s| s.success())
}

/// Ask a yes/no question (no by default). Without a terminal nobody can
/// answer, so that's an error pointing at `--yes`.
fn confirm(question: &str) -> Result<(), Failure> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return Err(Failure::Message(format!("{question} needs an answer: run it in a terminal, or pass --yes")));
    }
    print!("\n{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().lock().read_line(&mut answer);
    match answer.trim().to_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err(Failure::Message("cancelled; nothing was changed".into())),
    }
}

/// Print the commands of a plan, indented under `label` (blank after the first).
fn print_steps(label: &str, width: usize, steps: &[Step]) {
    for (i, step) in steps.iter().enumerate() {
        let label = if i == 0 { label } else { "" };
        println!("  {label:<width$}  {}", step.shown(ELEVATE));
    }
}

/// Run steps attached to the terminal, announcing each command.
fn run_in_terminal(steps: &[Step]) -> Result<Duration, action::Failed> {
    action::run(steps, ELEVATE, Io::Terminal, |event| {
        if let Event::Step(step) = event {
            let line = format!("$ {}", step.shown(ELEVATE));
            println!("{}", line.if_supports_color(Stdout, |l| l.cyan()));
        }
    })
}

fn section(title: &str) {
    println!("\n{}", format!("── {title} ").if_supports_color(Stdout, |t| t.bold()));
}

/// "8s", "2m 05s".
fn took(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 { format!("{secs}s") } else { format!("{}m {:02}s", secs / 60, secs % 60) }
}

enum Outcome {
    Updated(Duration),
    Failed(action::Failed),
    NoSudo,
}

fn update_all(detections: &[Detection], inventories: &[Inventory], dry_run: bool) -> Result<(), Failure> {
    let planned = update::plan(detections, inventories);
    let to_run: Vec<(&str, &[Step], bool)> = planned
        .iter()
        .filter_map(|p| match &p.plan {
            Plan::Run(steps) => Some((p.manager.as_str(), steps.as_slice(), p.needs_sudo())),
            Plan::Skip(_) => None,
        })
        .collect();
    let width = planned.iter().map(|p| p.manager.len()).max().unwrap_or(0);

    // The plan: every command, sudo ones included, before anything runs.
    if to_run.is_empty() {
        println!("Nothing to update.");
    } else {
        heading(&format!("{} to update\n", count(to_run.len(), "managers")));
        for (manager, steps, _) in &to_run {
            print_steps(manager, width, steps);
        }
    }
    let skipped: Vec<(&str, update::Skip)> = planned
        .iter()
        .filter_map(|p| match p.plan {
            Plan::Skip(why) => Some((p.manager.as_str(), why)),
            Plan::Run(_) => None,
        })
        .collect();
    if !skipped.is_empty() {
        println!("\n{}", "Skipped".if_supports_color(Stdout, |t| t.dimmed()));
        for (manager, why) in &skipped {
            let line = format!("  {manager:<width$}  {}", why.reason());
            println!("{}", line.if_supports_color(Stdout, |l| l.dimmed()));
        }
    }
    if dry_run {
        println!("\nDry run: nothing was changed.");
        return Ok(());
    }
    if to_run.is_empty() {
        return Ok(());
    }

    // Ask for the password once, up front, rather than in the middle of
    // another manager's output. If it's refused, the rest still runs.
    let needs_sudo: Vec<&str> = to_run.iter().filter(|(_, _, sudo)| *sudo).map(|(m, _, _)| *m).collect();
    let have_sudo = get_sudo(&needs_sudo);

    let mut outcomes: Vec<(&str, Outcome)> = Vec::new();
    for (i, (manager, steps, sudo)) in to_run.iter().enumerate() {
        section(&format!("{manager} ({}/{})", i + 1, to_run.len()));
        if *sudo && !have_sudo {
            println!("{}", "skipped: sudo wasn't given".if_supports_color(Stdout, |t| t.yellow()));
            outcomes.push((manager, Outcome::NoSudo));
            continue;
        }
        let result = action::run(steps, ELEVATE, Io::Stream, |event| match event {
            Event::Step(step) => {
                let line = format!("$ {}", step.shown(ELEVATE));
                println!("{}", line.if_supports_color(Stdout, |l| l.cyan()));
            }
            Event::Output(line) => println!("  {line}"),
        });
        outcomes.push((manager, match result {
            Ok(time) => Outcome::Updated(time),
            Err(failed) => Outcome::Failed(failed),
        }));
    }

    heading("\nSummary");
    for (manager, outcome) in &outcomes {
        let line = match outcome {
            Outcome::Updated(time) => format!("✓ {manager:<width$}  updated in {}", took(*time))
                .if_supports_color(Stdout, |l| l.green())
                .to_string(),
            Outcome::Failed(f) => format!("✗ {manager:<width$}  `{}` {}", f.step.shown(ELEVATE), f.reason)
                .if_supports_color(Stdout, |l| l.red())
                .to_string(),
            Outcome::NoSudo => format!("– {manager:<width$}  skipped: sudo wasn't given")
                .if_supports_color(Stdout, |l| l.yellow())
                .to_string(),
        };
        println!("  {line}");
    }
    let failed: Vec<&str> = outcomes
        .iter()
        .filter(|(_, o)| matches!(o, Outcome::Failed(_)))
        .map(|(m, _)| *m)
        .collect();
    match failed.as_slice() {
        [] => Ok(()),
        _ => Err(Failure::Message(format!("couldn't update {}", failed.join(", ")))),
    }
}

fn add(name: &str, via: Option<&str>, dry_run: bool, yes: bool, recipes: &[Recipe], env: &Env) -> Result<(), Failure> {
    let detections = detect_all(recipes, env, false);
    if detections.iter().any(|d| d.recipe.name == name && d.is_installed()) {
        println!("{name} is already installed.");
        return Ok(());
    }
    let plan = manage::plan_add(name, via, &detections).map_err(Failure::Message)?;
    let width = plan.iter().map(|a| a.manager.len()).max().unwrap_or(0);
    heading(&format!("Installing {name}\n"));
    for a in &plan {
        print_steps(&a.manager, width, &a.steps);
    }
    let first: Vec<&str> = plan[..plan.len() - 1].iter().map(|a| a.manager.as_str()).collect();
    if !first.is_empty() {
        println!(
            "\n{name} is installed via {}, which isn't installed yet, so that comes first.",
            first.join(" and ")
        );
    }
    if dry_run {
        println!("\nDry run: nothing was changed.");
        return Ok(());
    }
    if !first.is_empty() && !yes {
        confirm(&format!("Install {} too?", first.join(" and ")))?;
    }
    let sudo: Vec<&str> = plan
        .iter()
        .filter(|a| a.steps.iter().any(|s| s.sudo))
        .map(|a| a.manager.as_str())
        .collect();
    if !get_sudo(&sudo) {
        return Err(Failure::Message("sudo wasn't given; nothing was changed".into()));
    }

    for a in &plan {
        section(&format!("installing {} via {}", a.manager, a.method));
        if let Err(f) = run_in_terminal(&a.steps) {
            return Err(Failure::Message(format!("`{}` {}; {} wasn't installed", f.step.shown(ELEVATE), f.reason, a.manager)));
        }
    }

    // A new manager is often only on PATH in a new shell (rustup, brew).
    let after = detect_all(recipes, env, false);
    println!();
    for a in &plan {
        if after.iter().any(|d| d.recipe.name == a.manager && d.is_installed()) {
            println!("{}", format!("✓ {} installed", a.manager).if_supports_color(Stdout, |l| l.green()));
        } else {
            println!(
                "{}",
                format!(
                    "✓ {} installed, but isn't on PATH yet: open a new terminal, or follow its instructions above",
                    a.manager
                )
                .if_supports_color(Stdout, |l| l.yellow())
            );
        }
    }
    Ok(())
}

fn remove(name: &str, dry_run: bool, yes: bool, detections: &[Detection], env: &Env, cache: &Cache) -> Result<(), Failure> {
    let plan = manage::plan_remove(name, detections, env).map_err(Failure::Message)?;

    // Its packages and those of managers installed through it, read fresh:
    // this list is what `pmm restore` will have to go on.
    let involved: Vec<Detection> = detections
        .iter()
        .filter(|d| d.recipe.name == name || plan.dependents.contains(&d.recipe.name))
        .cloned()
        .collect();
    let inventories = list_all(&involved, env, cache, true);
    let refs: Vec<&Inventory> = inventories.iter().collect();
    let manifest = manifest::Manifest::from_inventories(name, &refs)
        .map_err(|e| Failure::Message(format!("{e}, so pmm can't save a list of them; not removing {name}")))?;

    let how = if detections.iter().any(|d| d.recipe.name == plan.method) {
        format!("via {}", plan.method)
    } else {
        format!("with its {} installer", plan.method)
    };
    heading(&format!("Removing {name} (installed {how})\n"));
    let mut inventories = inventories;
    inventories.sort_by_key(|i| i.manager != name);
    let width = inventories.iter().map(|i| i.manager.len()).max().unwrap_or(0);
    for inv in &inventories {
        let packages = inv.packages.as_ref().map(Vec::as_slice).unwrap_or_default();
        // nvm's are all "node"; tell them apart by version.
        let same_names = packages.iter().enumerate().any(|(i, p)| packages[..i].iter().any(|q| q.name == p.name));
        let labels: Vec<String> = packages
            .iter()
            .map(|p| match (&p.version, same_names) {
                (Some(v), true) => format!("{} {v}", p.name),
                _ => p.name.clone(),
            })
            .collect();
        let names: Vec<&str> = labels.iter().map(String::as_str).collect();
        let what = if names.is_empty() {
            format!("no {}", inv.items)
        } else {
            format!("{}: {}", count(names.len(), &inv.items), some_names(&names, false))
        };
        let manager = format!("{:<width$}", inv.manager);
        println!("  {}  {what}", manager.if_supports_color(Stdout, |m| m.cyan()));
    }
    if !plan.dependents.is_empty() {
        let line = format!(
            "\n⚠ {} {} installed via {name} and will stop working.",
            plan.dependents.join(", "),
            if plan.dependents.len() == 1 { "was" } else { "were" }
        );
        println!("{}", line.if_supports_color(Stdout, |l| l.yellow()));
    }
    if !plan.steps.is_empty() {
        println!("\nRuns");
        for step in &plan.steps {
            println!("  {}", step.shown(ELEVATE));
        }
    }
    if !plan.paths.is_empty() {
        println!("\nThen deletes");
        for path in &plan.paths {
            println!("  {}", tilde(path, env));
        }
    }
    if let Some(note) = &plan.note {
        println!("\nAfterwards: {note}");
    }
    if !manifest.packages.is_empty() {
        println!("\nThe list above is saved first, so `pmm restore` can reinstall it.");
    }
    if dry_run {
        println!("\nDry run: nothing was changed.");
        return Ok(());
    }
    if !yes {
        confirm(&format!("Remove {name} and everything listed?"))?;
    }

    let saved = match manifest.packages.is_empty() {
        true => None,
        false => {
            let dir = manifest::default_dir().ok_or_else(|| Failure::Message("no home folder to save the list in".into()))?;
            let path = manifest::save(&dir, &manifest)
                .map_err(|e| Failure::Message(format!("{e:#}; not removing {name}")))?;
            println!("Saved the list to {}", tilde(&path, env));
            Some(path)
        }
    };
    let restore_hint = match &saved {
        Some(path) => format!(" The package list is in {}.", tilde(path, env)),
        None => String::new(),
    };
    if plan.steps.iter().any(|s| s.sudo) && !get_sudo(&[name]) {
        return Err(Failure::Message("sudo wasn't given; nothing was removed".into()));
    }
    if let Err(f) = run_in_terminal(&plan.steps) {
        return Err(Failure::Message(format!(
            "`{}` {}; {name} may be partly removed.{restore_hint}",
            f.step.shown(ELEVATE),
            f.reason
        )));
    }
    let mut leftovers = Vec::new();
    for path in &plan.paths {
        match manage::delete_path(path) {
            Ok(()) => println!("Deleted {}", tilde(path, env)),
            Err(e) => leftovers.push(format!("{}: {e}", tilde(path, env))),
        }
    }

    println!();
    println!("{}", format!("✓ Removed {name}").if_supports_color(Stdout, |l| l.green()));
    if let Some(path) = &saved {
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        println!("  Bring its packages back with `pmm restore {stem}`.");
    }
    if let Some(note) = &plan.note {
        println!("  {note}");
    }
    if !leftovers.is_empty() {
        return Err(Failure::Message(format!("couldn't delete {}", leftovers.join("; "))));
    }
    Ok(())
}

fn saved_lists(env: &Env) {
    let saved = manifest::default_dir().map(|d| manifest::saved(&d)).unwrap_or_default();
    if saved.is_empty() {
        println!("No saved package lists. pmm saves one whenever it removes a manager.");
        return;
    }
    heading("Saved package lists (newest first)\n");
    let names: Vec<String> = saved
        .iter()
        .map(|(p, _)| p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())
        .collect();
    let width = names.iter().map(String::len).max().unwrap_or(0);
    for (name, (_, m)) in names.iter().zip(&saved) {
        println!("  {name:<width$}  {}", count(m.packages.len(), "packages"));
    }
    if let Some(dir) = manifest::default_dir() {
        let line = format!("\nIn {}. `pmm restore <name>` reinstalls one.", tilde(&dir, env));
        println!("{}", line.if_supports_color(Stdout, |l| l.dimmed()));
    }
}

fn restore(
    list: &str,
    via: Option<&str>,
    dry_run: bool,
    detections: &[Detection],
    env: &Env,
    cache: &Cache,
) -> Result<(), Failure> {
    let dir = manifest::default_dir();
    let path = manifest::resolve(list, dir.as_deref());
    let manifest = manifest::load(&path).map_err(|e| Failure::Message(format!("{e:#}")))?;
    let inventories = list_all(detections, env, cache, false);
    let items = manage::plan_restore(&manifest, via, detections, &inventories);

    heading(&format!(
        "Restoring {} removed with {} on {}\n",
        count(items.len(), "packages"),
        manifest.manager,
        manifest.date
    ));
    let to_install: Vec<(&manage::RestoreItem, &Step)> = items
        .iter()
        .filter_map(|i| match &i.restore {
            Restore::Install(step) => Some((i, step)),
            _ => None,
        })
        .collect();
    if to_install.is_empty() {
        println!("  Nothing to install.");
    }
    for (_, step) in &to_install {
        println!("  {}", step.shown(ELEVATE));
    }
    let already: Vec<&str> = items
        .iter()
        .filter(|i| i.restore == Restore::AlreadyInstalled)
        .map(|i| i.package.as_str())
        .collect();
    if !already.is_empty() {
        let line = format!("\nAlready installed: {}", some_names(&already, false));
        println!("{}", line.if_supports_color(Stdout, |l| l.dimmed()));
    }
    let cannot: Vec<(&manage::RestoreItem, &String)> = items
        .iter()
        .filter_map(|i| match &i.restore {
            Restore::Cannot(why) => Some((i, why)),
            _ => None,
        })
        .collect();
    if !cannot.is_empty() {
        println!("\n{}", "Can't restore".if_supports_color(Stdout, |l| l.yellow()));
        // Usually one reason for all of them ("pip can't install packages").
        for (why, names) in group_by(cannot.iter().map(|(i, why)| (*why, i.package.as_str()))) {
            println!("  {}: {why}", some_names(&names, false));
        }
        if via.is_none() {
            println!("  Try `--via <manager>` to install them with another manager.");
        }
    }
    if dry_run || to_install.is_empty() {
        if dry_run {
            println!("\nDry run: nothing was changed.");
        }
        return Ok(());
    }
    let sudo: Vec<&str> = to_install
        .iter()
        .filter(|(_, s)| s.sudo)
        .map(|(i, _)| i.manager.as_str())
        .fold(Vec::new(), |mut v, m| {
            if !v.contains(&m) {
                v.push(m);
            }
            v
        });
    let have_sudo = get_sudo(&sudo);

    let mut failed = Vec::new();
    for (n, (item, step)) in to_install.iter().enumerate() {
        section(&format!("{} ({}/{})", item.package, n + 1, to_install.len()));
        if step.sudo && !have_sudo {
            println!("skipped: sudo wasn't given");
            failed.push(item.package.as_str());
            continue;
        }
        if let Err(f) = run_in_terminal(std::slice::from_ref(*step)) {
            println!("{}", format!("✗ {}", f.reason).if_supports_color(Stdout, |l| l.red()));
            failed.push(item.package.as_str());
        }
    }
    println!();
    let done = to_install.len() - failed.len();
    println!("{}", format!("✓ Restored {}", count(done, "packages")).if_supports_color(Stdout, |l| l.green()));
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Failure::Message(format!("couldn't restore {}", failed.join(", "))))
    }
}

/// Cut `text` to `max` characters, with an ellipsis.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", text[..i].trim_end()),
        None => text.to_string(),
    }
}

/// Outdated packages, printed per manager as each one answers.
fn outdated(detections: &[Detection], inventories: &[Inventory], manager: Option<&str>) -> Result<(), Failure> {
    let checking = pmm_core::outdated::checkable(detections, inventories);
    if checking.is_empty() {
        return Err(Failure::Message(match manager {
            Some(name) => format!("pmm can't check {name} for newer versions"),
            None => "none of the installed managers can be checked for newer versions".into(),
        }));
    }
    let names: Vec<&str> = checking.iter().map(|d| d.recipe.name.as_str()).collect();
    let line = format!("Checking {}…", names.join(", "));
    println!("{}", line.if_supports_color(Stdout, |l| l.dimmed()));

    let mut current: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut outdated_in: Vec<String> = Vec::new();
    pmm_core::outdated::check_all(&checking, inventories, |checked| {
        let newer = match checked.newer {
            Ok(newer) if newer.is_empty() => return current.push(checked.manager),
            Ok(newer) => newer,
            Err(e) => return failed.push(format!("{}: {e}", checked.manager)),
        };
        let took = format!("{:.1}s", checked.took.as_secs_f32());
        println!(
            "\n{} {}  {}",
            checked.manager.if_supports_color(Stdout, |m| m.cyan()),
            format!("({})", newer.len()).if_supports_color(Stdout, |n| n.dimmed()),
            took.if_supports_color(Stdout, |t| t.dimmed())
        );
        let name_width = newer.iter().map(|n| n.name.len()).max().unwrap_or(0);
        let versions: Vec<String> = newer.iter().map(|n| clip(n.version.as_deref().unwrap_or(""), 24)).collect();
        let version_width = versions.iter().map(|v| v.chars().count()).max().unwrap_or(0);
        for (n, version) in newer.iter().zip(&versions) {
            let latest = match (&n.version, &n.latest) {
                (Some(v), Some(l)) if v == l => "new build",
                (_, Some(l)) => l.as_str(),
                (_, None) => "newer version",
            };
            let line = if version.is_empty() {
                format!("  {:<name_width$}  → {}", n.name, latest.if_supports_color(Stdout, |l| l.green()))
            } else {
                format!("  {:<name_width$}  {version:<version_width$} → {}", n.name, latest.if_supports_color(Stdout, |l| l.green()))
            };
            println!("{line}");
        }
        outdated_in.push(checked.manager);
    });

    if !current.is_empty() {
        current.sort();
        let line = format!("\nUp to date: {}", current.join(", "));
        println!("{}", line.if_supports_color(Stdout, |l| l.dimmed()));
    }
    for f in &failed {
        eprintln!("warning: couldn't check {f}");
    }
    if outdated_in.is_empty() {
        if failed.is_empty() {
            println!("Everything is up to date.");
        }
    } else {
        let one = if outdated_in.len() == 1 { outdated_in[0].as_str() } else { "<manager>" };
        let hint = format!(
            "\nUpdate: `pmm update` (everything), `pmm update -m {one}`, or `pmm upgrade <package> -m {one}`."
        );
        println!("{}", hint.if_supports_color(Stdout, |h| h.dimmed()));
    }
    if names.contains(&"apt") {
        let note = "apt's answer is as of the last `apt update` (`pmm update -m apt` runs it).";
        println!("{}", note.if_supports_color(Stdout, |n| n.dimmed()));
    }
    Ok(())
}

/// Repository results, printed per manager as each one answers. False if
/// none had anything.
fn search_repositories(
    detections: &[Detection],
    inventories: &[Inventory],
    query: &str,
    all: bool,
    after_installed: bool,
) -> Result<bool, Failure> {
    const SHOWN: usize = 8;
    let searching: Vec<&str> = remote::searchable(detections).iter().map(|d| d.recipe.name.as_str()).collect();
    if searching.is_empty() {
        println!("{}", "None of these managers can search a repository.".if_supports_color(Stdout, |l| l.dimmed()));
        return Ok(false);
    }
    if after_installed {
        println!();
    }
    heading("In repositories");
    let line = format!("Asking {}…", searching.join(", "));
    println!("{}", line.if_supports_color(Stdout, |l| l.dimmed()));

    let mut empty: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut any = false;
    remote::search_remote(detections, inventories, query, |results| {
        let found = match results.found {
            Ok(found) if !found.is_empty() => found,
            Ok(_) => {
                empty.push(if results.exact_only {
                    format!("{} (exact names only)", results.manager)
                } else {
                    results.manager
                });
                return;
            }
            Err(e) => {
                failed.push(format!("{}: {e}", results.manager));
                return;
            }
        };
        any = true;
        let shown = if all { found.len() } else { found.len().min(SHOWN) };
        let name_width = found[..shown].iter().map(|f| f.package.name.len()).max().unwrap_or(0);
        let versions: Vec<String> = found[..shown]
            .iter()
            .map(|f| clip(f.package.version.as_deref().unwrap_or(""), 16))
            .collect();
        let version_width = versions.iter().map(|v| v.chars().count()).max().unwrap_or(0);
        // Only make room for the "installed" mark where something is.
        let mark_width = if found[..shown].iter().any(|f| f.installed) { 9 } else { 0 };
        let took = format!("{:.1}s", results.took.as_secs_f32());
        println!(
            "\n{}  {}",
            results.manager.if_supports_color(Stdout, |m| m.cyan()),
            took.if_supports_color(Stdout, |t| t.dimmed())
        );
        for (f, version) in found[..shown].iter().zip(&versions) {
            let name = format!("{:<name_width$}", f.package.name);
            let name = if f.rank == Rank::Exact {
                name.if_supports_color(Stdout, |n| n.bold()).to_string()
            } else {
                name
            };
            let version = format!("{version:<version_width$}");
            let mark = if f.installed { "installed" } else { "" };
            let about = [f.package.title.as_deref(), f.package.note.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            let mark = format!("{mark:<mark_width$}");
            let line = format!(
                "  {name}  {version}  {}{}{}",
                mark.if_supports_color(Stdout, |m| m.green()),
                if mark_width > 0 { "  " } else { "" },
                clip(&about, 70).if_supports_color(Stdout, |a| a.dimmed())
            );
            println!("{}", line.trim_end());
        }
        if shown < found.len() {
            let more = format!("  +{} more (`pmm search {query} -m {}`)", found.len() - shown, results.manager);
            println!("{}", more.if_supports_color(Stdout, |m| m.dimmed()));
        }
    })
    .map_err(Failure::Message)?;

    if !empty.is_empty() {
        let line = format!("\nNo results: {}", empty.join(", "));
        println!("{}", line.if_supports_color(Stdout, |l| l.dimmed()));
    }
    for f in &failed {
        eprintln!("warning: couldn't search {f}");
    }
    Ok(any)
}

fn info(detections: &[Detection], inventories: &[Inventory], package: &str, one: bool) -> Result<(), Failure> {
    const LINES: usize = 15;
    let key = dupe_key(package);
    let installed: Vec<String> = inventories
        .iter()
        .flat_map(|inv| {
            inv.packages.iter().flatten().filter(|p| dupe_key(&p.name) == key).map(move |p| {
                format!("{} {}", inv.manager, p.version.as_deref().unwrap_or("")).trim_end().to_string()
            })
        })
        .collect();
    heading(package);
    match installed.as_slice() {
        [] => println!("Not installed."),
        list => println!("Installed: {}", list.join(" · ")),
    }

    let mut answers = remote::info_all(detections, package);
    // Managers that have it installed first.
    answers.sort_by_key(|(m, _)| !installed.iter().any(|i| i.split(' ').next() == Some(m)));
    let mut missing = Vec::new();
    let mut any = false;
    // pip, pipx and uv all ask PyPI: one block, headed by all three.
    let answers: Vec<(String, Result<String, String>)> = group_by(answers.into_iter().map(|(m, a)| (a, m)))
        .into_iter()
        .flat_map(|(answer, managers)| match answer {
            Ok(text) => vec![(managers.join(" · "), Ok(text))],
            Err(e) => managers.into_iter().map(|m| (m.to_string(), Err(e.clone()))).collect(),
        })
        .collect();
    for (manager, answer) in &answers {
        match answer {
            Ok(text) => {
                any = true;
                section(manager);
                let lines: Vec<&str> = text.lines().collect();
                let shown = if one { lines.len() } else { lines.len().min(LINES) };
                for line in &lines[..shown] {
                    println!("{line}");
                }
                if shown < lines.len() {
                    let more = format!("… {} more lines (`pmm info {package} -m {manager}`)", lines.len() - shown);
                    println!("{}", more.if_supports_color(Stdout, |m| m.dimmed()));
                }
            }
            Err(e) if one => eprintln!("{manager}: {e}"),
            Err(_) => missing.push(manager.as_str()),
        }
    }
    if !missing.is_empty() {
        let line = format!("\nNot found by: {}", missing.join(", "));
        println!("{}", line.if_supports_color(Stdout, |l| l.dimmed()));
    }
    if any || !installed.is_empty() { Ok(()) } else { Err(Failure::NoMatches) }
}

/// Ask for one of `n` numbered options (already printed). Without a
/// terminal nobody can answer, so that's an error pointing at `-m`.
fn choose(question: &str, n: usize) -> Result<usize, Failure> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return Err(Failure::Message(format!("{question} needs an answer: run it in a terminal, or pick with --manager")));
    }
    let range = if n == 1 { "1".to_string() } else { format!("1-{n}") };
    print!("\n{question} [{range}, Enter to cancel] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().lock().read_line(&mut answer);
    match answer.trim().parse::<usize>() {
        Ok(i) if (1..=n).contains(&i) => Ok(i - 1),
        _ => Err(Failure::Message("cancelled; nothing was changed".into())),
    }
}

/// Run one package command, asking for sudo first if it needs it.
fn run_package_step(step: &Step, manager: &str) -> Result<(), Failure> {
    if step.sudo && !get_sudo(&[manager]) {
        return Err(Failure::Message("sudo wasn't given; nothing was changed".into()));
    }
    println!();
    run_in_terminal(std::slice::from_ref(step))
        .map(|_| ())
        .map_err(|f| Failure::Message(format!("`{}` {}", f.step.shown(ELEVATE), f.reason)))
}

fn install(detections: &[Detection], package: &str, manager: Option<&str>, env: &Env, cache: &Cache) -> Result<(), Failure> {
    if !action::is_package_name(package) {
        return Err(Failure::Message(format!("{package:?} isn't a package name pmm will pass to a command")));
    }
    // Managers that can install, limited to the one named.
    let able: Vec<Detection> = select_installed(detections.to_vec(), manager)?
        .into_iter()
        .filter(|d| d.is_installed() && d.recipe.install_pkg.is_some())
        .collect();
    if let (Some(m), true) = (manager, able.is_empty()) {
        return Err(Failure::Message(format!("pmm can't install {m} packages")));
    }
    let inventories = list_all(&able, env, cache, false);

    // Each manager with an exact match, and what its repository calls it.
    let mut options: Vec<(String, pmm_core::remote::Found)> = Vec::new();
    if let Some(m) = manager {
        // Named explicitly: trust the name, the manager will say if it's wrong.
        let installed = inventories
            .iter()
            .flat_map(|i| i.packages.iter().flatten())
            .any(|p| dupe_key(&p.name) == dupe_key(package));
        let found = pmm_core::remote::Found {
            package: pmm_core::Package { name: package.into(), version: None, title: None, note: None },
            rank: Rank::Exact,
            installed,
        };
        options.push((m.to_string(), found));
    } else {
        let asking: Vec<&str> = remote::searchable(&able).iter().map(|d| d.recipe.name.as_str()).collect();
        println!("{}", format!("Looking for {package} in {}…", asking.join(", ")).if_supports_color(Stdout, |l| l.dimmed()));
        let mut failed = Vec::new();
        remote::search_remote(&able, &inventories, package, |results| match results.found {
            Ok(found) => {
                for found in found.into_iter().filter(|f| remote::is_exact(f, package)) {
                    options.push((results.manager.clone(), found));
                }
            }
            Err(e) => failed.push(format!("{}: {e}", results.manager)),
        })
        .map_err(Failure::Message)?;
        for f in &failed {
            eprintln!("warning: couldn't search {f}");
        }
        if options.is_empty() {
            return Err(Failure::Message(format!(
                "no repository has a package called {package}; `pmm search {package}` shows similar names"
            )));
        }
        options.sort_by(|a, b| a.0.cmp(&b.0));
    }

    let (chosen_manager, found) = if manager.is_some() {
        &options[0]
    } else {
        heading(&format!("\n{package} is available from\n"));
        let width = options.iter().map(|(m, _)| m.len()).max().unwrap_or(0);
        let name_width = options.iter().map(|(_, f)| f.package.name.len()).max().unwrap_or(0);
        let versions: Vec<String> =
            options.iter().map(|(_, f)| clip(f.package.version.as_deref().unwrap_or(""), 16)).collect();
        let version_width = versions.iter().map(|v| v.chars().count()).max().unwrap_or(0);
        for (i, ((m, f), version)) in options.iter().zip(&versions).enumerate() {
            let p = &f.package;
            let about = [p.title.as_deref(), p.note.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
            let about = if f.installed { format!("(installed) {about}") } else { about };
            let line = format!(
                "  {}) {m:<width$}  {:<name_width$}  {version:<version_width$}  {}",
                i + 1,
                p.name,
                clip(&about, 60).if_supports_color(Stdout, |a| a.dimmed())
            );
            println!("{}", line.trim_end());
        }
        &options[choose("Install with which?", options.len())?]
    };
    if found.installed {
        println!("{} is already installed with {chosen_manager}.", found.package.name);
        return Ok(());
    }
    let d = able.iter().find(|d| d.recipe.name == *chosen_manager).expect("chosen from these");
    let action = d.recipe.install_pkg.as_ref().expect("filtered on install_pkg");
    let step = Step::for_package(action, &found.package.name, None)
        .ok_or_else(|| Failure::Message(format!("{:?} isn't a name pmm will pass to a command", found.package.name)))?;
    run_package_step(&step, chosen_manager)?;
    println!("{}", format!("\n✓ Installed {} with {chosen_manager}", found.package.name).if_supports_color(Stdout, |l| l.green()));
    Ok(())
}

/// The installed copies of `package` (by the duplicate rules), asking which
/// one if there are several. `what` is the verb for the question.
fn pick_installed(
    detections: &[Detection],
    package: &str,
    manager: Option<&str>,
    what: &str,
    env: &Env,
    cache: &Cache,
) -> Result<(Detection, pmm_core::Package), Failure> {
    let selected = select_installed(detections.to_vec(), manager)?;
    let inventories = list_all(&selected, env, cache, false);
    let key = dupe_key(package);
    let copies: Vec<(&str, &pmm_core::Package)> = inventories
        .iter()
        .flat_map(|inv| {
            inv.packages.iter().flatten().filter(|p| dupe_key(&p.name) == key).map(move |p| (inv.manager.as_str(), p))
        })
        .collect();
    let (m, p) = match copies.as_slice() {
        [] => {
            let place = manager.map(|m| format!(" with {m}")).unwrap_or_default();
            return Err(Failure::Message(format!("{package} isn't installed{place}")));
        }
        [one] => *one,
        several => {
            heading(&format!("{package} is installed more than once\n"));
            let width = several.iter().map(|(m, _)| m.len()).max().unwrap_or(0);
            for (i, (m, p)) in several.iter().enumerate() {
                println!("  {}) {m:<width$}  {} {}", i + 1, p.name, p.version.as_deref().unwrap_or(""));
            }
            several[choose(&format!("{what} which?"), several.len())?]
        }
    };
    let d = selected.iter().find(|d| d.recipe.name == m).expect("listed from these").clone();
    Ok((d, p.clone()))
}

fn uninstall(detections: &[Detection], package: &str, manager: Option<&str>, yes: bool, env: &Env, cache: &Cache) -> Result<(), Failure> {
    let (d, p) = pick_installed(detections, package, manager, "Uninstall", env, cache)?;
    let m = d.recipe.name.as_str();
    let action = d
        .recipe
        .remove_pkg
        .as_ref()
        .ok_or_else(|| Failure::Message(format!("pmm can't uninstall {m} packages")))?;
    let step = Step::for_package(action, &p.name, p.version.as_deref())
        .ok_or_else(|| Failure::Message(format!("{:?} isn't a name pmm will pass to a command", p.name)))?;

    // Uninstalling a manager this way loses track of its packages.
    let is_manager = detections.iter().find(|other| {
        other.recipe.name == dupe_key(&p.name) || other.recipe.name == p.name.trim_start_matches("python3-")
    });
    if let Some(other) = is_manager.filter(|o| matches!(&o.status, Status::Installed { via: Via::Manager(v), .. } if v == m)) {
        let line = format!(
            "⚠ This is how {} was installed. `pmm remove {}` also removes its packages and saves a list of them first.",
            other.recipe.name, other.recipe.name
        );
        println!("{}", line.if_supports_color(Stdout, |l| l.yellow()));
    }
    let label = format!("{} {}", p.name, p.version.as_deref().unwrap_or("")).trim_end().to_string();
    println!("Uninstalling {label} ({m})\n  {}", step.shown(ELEVATE));
    if !yes {
        confirm(&format!("Uninstall {label}?"))?;
    }
    run_package_step(&step, m)?;
    println!("{}", format!("\n✓ Uninstalled {label} ({m})").if_supports_color(Stdout, |l| l.green()));
    Ok(())
}

fn upgrade(detections: &[Detection], package: &str, manager: Option<&str>, env: &Env, cache: &Cache) -> Result<(), Failure> {
    let (d, p) = pick_installed(detections, package, manager, "Upgrade", env, cache)?;
    let m = d.recipe.name.as_str();
    let action = d
        .recipe
        .upgrade_pkg
        .as_ref()
        .ok_or_else(|| Failure::Message(format!("pmm can't upgrade {m} packages one at a time")))?;
    let step = Step::for_package(action, &p.name, p.version.as_deref())
        .ok_or_else(|| Failure::Message(format!("{:?} isn't a name pmm will pass to a command", p.name)))?;
    run_package_step(&step, m)?;
    println!("{}", format!("\n✓ Upgraded {} ({m})", p.name).if_supports_color(Stdout, |l| l.green()));
    Ok(())
}
