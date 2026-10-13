use std::collections::BTreeMap;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use owo_colors::{OwoColorize, Stream::Stdout};
use pmm_core::{
    Cache, Detection, Env, Inventory, Rank, Source, Status, Via, detect_all, list_all, recipe,
    search,
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
