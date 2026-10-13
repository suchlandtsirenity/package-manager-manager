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
use pmm_core::{Recipe, manifest};

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
