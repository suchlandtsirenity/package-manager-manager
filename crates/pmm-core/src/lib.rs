//! pmm-core: everything pmm knows and does. The CLI and GUI are thin layers
//! over this crate.

mod builtins;
pub mod cache;
pub mod detect;
pub mod inventory;
pub mod paths;
pub mod process;
pub mod recipe;

pub use cache::Cache;
pub use detect::{Detection, Status, Via, detect_all};
pub use inventory::{Hit, Inventory, Package, Rank, list_all, search};
pub use paths::Env;
pub use recipe::{Loaded, Recipe, Source};
