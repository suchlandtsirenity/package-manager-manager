use std::process::ExitCode;

use clap::{Parser, Subcommand};
use owo_colors::{OwoColorize, Stream::Stdout};
use pmm_core::{Detection, Env, Source, Status, Via, detect_all, recipe};

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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let loaded = recipe::load(recipe::default_user_dir().as_deref());
    for warning in &loaded.warnings {
        eprintln!("warning: skipped recipe {warning}");
    }

    match cli.command {
        Cmd::Managers => managers(&detect_all(&loaded.recipes, &Env::current())),
    }
    ExitCode::SUCCESS
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
