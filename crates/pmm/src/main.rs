use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::SystemTime;

use clap::{Parser, Subcommand};
use owo_colors::{OwoColorize, Stream::Stdout};
use pmm_core::{
    Cache, Detection, Env, Inventory, Owners, Rank, Source, Status, Via, cache, detect_all,
    list_all, package_dupes, recipe, search, shadowed_commands, which_runs,
};

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
    /// Find installed packages by name, across every manager.
    Search {
        query: String,
        /// Only search this manager.
        #[arg(short, long)]
        manager: Option<String>,
        /// Re-read from the managers instead of using the cache.
        #[arg(long)]
        refresh: bool,
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
        Cmd::Search { query, manager, refresh } => {
            select(detect_all(&loaded.recipes, &env, false), manager.as_deref()).and_then(|d| {
                let inventories = list_all(&d, &env, &cache, refresh);
                search_installed(&inventories, &query)
            })
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

/// Keep only `manager` if one was named, checking that it exists and is installed.
fn select(detections: Vec<Detection>, manager: Option<&str>) -> Result<Vec<Detection>, Failure> {
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
    if d.recipe.list.is_none() {
        return Err(Failure::Message(format!("pmm can't list {name}'s packages yet")));
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

fn search_installed(inventories: &[Inventory], query: &str) -> Result<(), Failure> {
    for inv in inventories {
        if let Err(e) = &inv.packages {
            eprintln!("warning: couldn't read {}: {e}", inv.manager);
        }
    }
    let hits = search(inventories, query);
    if hits.is_empty() {
        eprintln!("Nothing installed matches {query:?}.");
        return Err(Failure::NoMatches);
    }
    let n = hits.len();
    println!("{n} {} for {query:?}\n", if n == 1 { "match" } else { "matches" });

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
    Ok(())
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
