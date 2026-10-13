//! pmm-core: everything pmm knows and does. The CLI and GUI are thin layers
//! over this crate.

pub mod action;
pub mod binaries;
mod builtins;
pub mod cache;
pub mod detect;
pub mod dupes;
pub mod inventory;
pub mod manage;
pub mod manifest;
pub mod outdated;
pub mod paths;
pub mod process;
pub mod recipe;
pub mod remote;
pub mod update;

pub use cache::Cache;
pub use detect::{Detection, Owners, Status, Via, detect_all};
pub use dupes::{PackageDupe, Shadowed, package_dupes, shadowed_commands, which_runs};
pub use inventory::{Hit, Inventory, Package, Rank, list_all, search};
pub use paths::Env;
pub use recipe::{Loaded, Recipe, Source};
