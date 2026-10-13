//! What the GUI shows, read from pmm-core. Loading runs the managers' list
//! commands, so it happens off the UI thread (see `App::load`).

use std::collections::HashMap;

use pmm_core::{Cache, Detection, Env, Inventory, Package, detect_all, list_all, package_dupes, recipe};

/// One installed package, as a table row.
#[derive(Debug, Clone)]
pub struct Row {
    pub manager: String,
    pub package: Package,
    /// The other managers that have the same package.
    pub also: Vec<String>,
    /// The name lowercased once, for sorting.
    pub sort_name: String,
}

impl Row {
    /// Title and note, e.g. "Firefox" or "node v22.12.0".
    pub fn extra(&self) -> String {
        [self.package.title.as_deref(), self.package.note.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// An installed manager, for the sidebar.
#[derive(Debug, Clone)]
pub struct Manager {
    pub name: String,
    /// None if its list couldn't be read (or it has no `[list]`).
    pub count: Option<usize>,
}

#[derive(Debug, Default)]
pub struct Snapshot {
    /// Every known manager, with its recipe: what the GUI can do with it.
    pub detections: Vec<Detection>,
    /// Each manager's list, as read (for checks that need it, like updates).
    pub inventories: Vec<Inventory>,
    pub managers: Vec<Manager>,
    pub rows: Vec<Row>,
    /// Packages installed by more than one manager (as `pmm dupes` counts
    /// them; each has a row per copy).
    pub duplicate_packages: usize,
    /// Broken user recipes and lists that couldn't be read.
    pub problems: Vec<String>,
}

/// Everything installed, read the same way as `pmm list` (with the cache,
/// unless `refresh`).
pub fn load(refresh: bool) -> Snapshot {
    let loaded = recipe::load(recipe::default_user_dir().as_deref());
    let env = Env::current();
    let cache = Cache::new(Cache::default_dir());
    let detections = detect_all(&loaded.recipes, &env, false);
    let inventories = list_all(&detections, &env, &cache, refresh);

    let mut problems: Vec<String> = loaded.warnings.iter().map(|w| format!("Skipped recipe {w}")).collect();
    let mut managers: Vec<Manager> = detections
        .iter()
        .filter(|d| d.is_installed())
        .map(|d| {
            let inventory = inventories.iter().find(|i| i.manager == d.recipe.name);
            if let Some(Err(e)) = inventory.map(|i| &i.packages) {
                problems.push(format!("Couldn't read {}: {e}", d.recipe.name));
            }
            Manager {
                name: d.recipe.name.clone(),
                count: inventory.and_then(|i| i.packages.as_ref().ok()).map(Vec::len),
            }
        })
        .collect();
    managers.sort_by(|a, b| a.name.cmp(&b.name));

    // (manager, package name) -> the other managers with the same package.
    let mut also: HashMap<(String, String), Vec<String>> = HashMap::new();
    let dupes = package_dupes(&inventories);
    let duplicate_packages = dupes.len();
    for dupe in dupes {
        for (manager, package) in &dupe.copies {
            let others = dupe.copies.iter().map(|(m, _)| m.to_string()).filter(|m| m != manager).collect();
            also.insert((manager.to_string(), package.name.clone()), others);
        }
    }

    let rows = inventories
        .iter()
        .filter_map(|inv| Some((inv, inv.packages.as_ref().ok()?)))
        .flat_map(|(inv, packages)| {
            packages.iter().map(|p| Row {
                sort_name: p.name.to_lowercase(),
                manager: inv.manager.clone(),
                also: also.get(&(inv.manager.clone(), p.name.clone())).cloned().unwrap_or_default(),
                package: p.clone(),
            })
        })
        .collect();
    Snapshot { detections, inventories, managers, rows, duplicate_packages, problems }
}

impl Snapshot {
    pub fn detection(&self, manager: &str) -> Option<&Detection> {
        self.detections.iter().find(|d| d.recipe.name == manager)
    }
}
