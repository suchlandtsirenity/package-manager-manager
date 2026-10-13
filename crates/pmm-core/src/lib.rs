//! pmm-core: everything pmm knows and does. The CLI and GUI are thin layers
//! over this crate.

pub mod detect;
pub mod recipe;

pub use detect::{Detection, Env, Status, Via, detect_all};
pub use recipe::{Loaded, Recipe, Source};
