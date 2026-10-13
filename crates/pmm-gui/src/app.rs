//! The window: a sidebar of managers, one searchable, sortable table of
//! everything installed (duplicates flagged), a package page in the side
//! drawer with Upgrade and Uninstall, and an Activity page with the live
//! output of what pmm ran.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use cosmic::app::context_drawer::{self, ContextDrawer};
use cosmic::app::{Core, Task};
use cosmic::iced::futures::channel::oneshot;
use cosmic::iced::widget::text::Wrapping;
use cosmic::iced::{Alignment, Length};
use cosmic::prelude::*;
use cosmic::widget::{self, nav_bar};
use pmm_core::action::{self, Step};
use pmm_core::detect::{Status, Via};
use pmm_core::dupes::dupe_key;
use pmm_core::inventory::rank;
use pmm_core::recipe::PackageAction;
use pmm_core::remote::RemoteResults;
use pmm_core::{Rank, remote};

use crate::data::{self, Row, Snapshot};
use crate::jobs::{self, Job, JobEvent, State};

/// Rows drawn at once. Thousands of rows make every redraw slow, and nobody
/// scrolls through 3000 packages: they search.
const SHOWN: usize = 400;

/// Column widths, as shares of the table's width. Manager needs room for
/// "Manager ▼" on one line.
const NAME: u16 = 8;
const VERSION: u16 = 5;
const MANAGER: u16 = 3;
const NOTES: u16 = 6;

/// Repository results shown per manager before "Show all".
const REMOTE_SHOWN: usize = 8;

/// Info lines shown on a package page.
const INFO_LINES: usize = 40;

/// What a sidebar entry shows.
#[derive(Debug, Clone, PartialEq)]
enum Page {
    All,
    Duplicates,
    Manager(String),
    Activity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Name,
    Version,
    Manager,
}

/// One installed copy of a package.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub manager: String,
    pub name: String,
    pub version: Option<String>,
}

impl Key {
    fn of(row: &Row) -> Key {
        Key { manager: row.manager.clone(), name: row.package.name.clone(), version: row.package.version.clone() }
    }

    /// "pipx 1.4.3-1 (apt)".
    fn label(&self) -> String {
        match &self.version {
            Some(v) => format!("{} {v} ({})", self.name, self.manager),
            None => format!("{} ({})", self.name, self.manager),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verb {
    Install,
    Upgrade,
    Uninstall,
}

/// Which half of a search is showing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tab {
    Installed,
    Repositories,
}

/// A search of every manager's repository, filled in as each answers.
struct RemoteSearch {
    /// Which search this is; answers to an older one are dropped.
    id: usize,
    query: String,
    results: Vec<RemoteResults>,
    /// Managers that haven't answered yet.
    waiting: Vec<String>,
    /// Managers whose results are all shown, not just the best few.
    expanded: Vec<String>,
}

impl Verb {
    fn action(self, recipe: &pmm_core::Recipe) -> Option<&PackageAction> {
        match self {
            Verb::Install => recipe.install_pkg.as_ref(),
            Verb::Upgrade => recipe.upgrade_pkg.as_ref(),
            Verb::Uninstall => recipe.remove_pkg.as_ref(),
        }
    }

    fn word(self) -> &'static str {
        match self {
            Verb::Install => "Install",
            Verb::Upgrade => "Upgrade",
            Verb::Uninstall => "Uninstall",
        }
    }
}

/// What a dry run said an uninstall would take with it.
#[derive(Debug, Clone)]
enum Preview {
    Checking,
    /// Every package it would remove; None if the recipe has no dry run.
    Ready(Option<Vec<String>>),
    Refused(String),
}

/// The "are you sure?" dialog for an uninstall.
struct Confirm {
    key: Key,
    preview: Preview,
    /// Set when the package is how a manager was installed (pipx via apt).
    manager_warning: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Loaded(Arc<Snapshot>),
    Search(String),
    ClearSearch,
    Refresh,
    Sort(Column),
    Open(Key),
    CloseDrawer,
    InfoLoaded(Key, Result<String, String>),
    Run(Verb, Key),
    PreviewLoaded(Key, Result<Option<Vec<String>>, String>),
    Confirmed,
    Cancelled,
    Job(usize, JobEvent),
    ShowTab(Tab),
    SearchRepositories,
    RemoteAnswer(usize, Box<RemoteResults>),
    ShowAll(String),
}

pub struct App {
    core: Core,
    nav: nav_bar::Model,
    snapshot: Option<Arc<Snapshot>>,
    loading: bool,
    query: String,
    sort: Column,
    ascending: bool,
    /// The package whose page is open in the drawer.
    selected: Option<Key>,
    info: HashMap<Key, Option<Result<String, String>>>,
    confirm: Option<Confirm>,
    jobs: Vec<Job>,
    /// Indexes into the snapshot's rows for the current page, search and
    /// sort: worked out when one of those changes, not on every redraw.
    visible: Vec<usize>,
    tab: Tab,
    remote: Option<RemoteSearch>,
}

/// Run blocking work (a manager's command) on its own thread, so the window
/// keeps responding.
async fn off_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.await.ok()
}

impl App {
    fn load(&mut self, refresh: bool) -> Task<Message> {
        self.loading = true;
        cosmic::task::future(async move {
            let snapshot = off_thread(move || data::load(refresh)).await.unwrap_or_default();
            Message::Loaded(Arc::new(snapshot))
        })
    }

    /// Rebuild the sidebar from a new snapshot, keeping the selected page.
    fn fill_nav(&mut self, snapshot: &Snapshot) {
        let selected = self.page();
        self.nav.clear();
        let all = self.nav.insert().text(format!("All packages  {}", snapshot.rows.len())).data(Page::All).id();
        self.nav
            .insert()
            .text(format!("Duplicates  {}", snapshot.duplicate_packages))
            .data(Page::Duplicates);
        let mut first = true;
        for m in &snapshot.managers {
            let count = m.count.map_or("?".to_string(), |n| n.to_string());
            self.nav
                .insert()
                .text(format!("{}  {count}", m.name))
                .data(Page::Manager(m.name.clone()))
                .divider_above(std::mem::take(&mut first));
        }
        self.nav.insert().text("Activity").data(Page::Activity).divider_above(true);
        let keep = self
            .nav
            .iter()
            .find(|id| self.nav.data::<Page>(*id) == Some(&selected))
            .unwrap_or(all);
        self.nav.activate(keep);
    }

    fn page(&self) -> Page {
        self.nav.active_data::<Page>().cloned().unwrap_or(Page::All)
    }

    /// Work out which rows show, best matches first when searching,
    /// otherwise in the chosen column order.
    fn refilter(&mut self) {
        let Some(snapshot) = self.snapshot.clone() else { return };
        let page = self.page();
        let query = self.query.trim().to_lowercase();
        let mut rows: Vec<(usize, Option<Rank>)> = snapshot
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| match &page {
                Page::All | Page::Activity => true,
                Page::Duplicates => !r.also.is_empty(),
                Page::Manager(m) => &r.manager == m,
            })
            .filter_map(|(i, r)| match query.is_empty() {
                true => Some((i, None)),
                false => rank(&r.package, &query).map(|k| (i, Some(k))),
            })
            .collect();
        let rows_of = &snapshot.rows;
        rows.sort_by(|(a, ra), (b, rb)| ra.cmp(rb).then_with(|| self.compare(&rows_of[*a], &rows_of[*b])));
        self.visible = rows.into_iter().map(|(i, _)| i).collect();
    }

    fn compare(&self, a: &Row, b: &Row) -> Ordering {
        let by_name = |a: &Row, b: &Row| a.sort_name.cmp(&b.sort_name);
        let order = match self.sort {
            Column::Name => by_name(a, b).then_with(|| a.manager.cmp(&b.manager)),
            Column::Version => a.package.version.cmp(&b.package.version).then_with(|| by_name(a, b)),
            Column::Manager => a.manager.cmp(&b.manager).then_with(|| by_name(a, b)),
        };
        if self.ascending { order } else { order.reverse() }
    }

    /// Every installed copy of the package `key` is a copy of, by the
    /// duplicate rules (so pip's `PyYAML` and apt's `pyyaml` both show).
    fn copies<'a>(&self, snapshot: &'a Snapshot, key: &Key) -> Vec<&'a Row> {
        let wanted = dupe_key(&key.name);
        let mut copies: Vec<&Row> = snapshot.rows.iter().filter(|r| dupe_key(&r.package.name) == wanted).collect();
        copies.sort_by(|a, b| (a.manager != key.manager, &a.manager).cmp(&(b.manager != key.manager, &b.manager)));
        copies
    }

    fn open(&mut self, key: Key) -> Task<Message> {
        self.selected = Some(key.clone());
        self.set_show_context(true);
        if self.info.contains_key(&key) {
            return Task::none();
        }
        let Some(detection) = self.snapshot.as_ref().and_then(|s| s.detection(&key.manager)).cloned() else {
            return Task::none();
        };
        self.info.insert(key.clone(), None);
        cosmic::task::future(async move {
            let name = key.name.clone();
            let answer = off_thread(move || {
                let mut answers = remote::info_all(std::slice::from_ref(&detection), &name);
                answers.pop().map(|(_, a)| a).unwrap_or(Err("pmm can't show details for this manager".into()))
            })
            .await
            .unwrap_or(Err("pmm crashed while asking".into()));
            Message::InfoLoaded(key, answer)
        })
    }

    /// The command a verb runs for a copy, with the recipe's `unattended`
    /// arguments: the GUI asked first, and nobody can answer the command.
    fn step(&self, verb: Verb, key: &Key) -> Result<Step, String> {
        let snapshot = self.snapshot.as_ref().ok_or("still loading")?;
        let recipe = &snapshot.detection(&key.manager).ok_or("unknown manager")?.recipe;
        let action = verb.action(recipe).ok_or_else(|| format!("pmm can't {} {} packages", verb.word().to_lowercase(), key.manager))?;
        let step = Step::for_package(action, &key.name, key.version.as_deref())
            .ok_or("not a name pmm will pass to a command")?;
        Ok(step.with_args(&action.unattended))
    }

    fn start(&mut self, verb: Verb, key: &Key) -> Task<Message> {
        let step = match self.step(verb, key) {
            Ok(step) => step,
            Err(e) => {
                let mut job = Job::new(format!("{} {}", verb.word(), key.label()), &Step { command: vec![], sudo: false }, &key.manager, &key.name);
                job.state = State::Failed(e);
                self.jobs.push(job);
                return Task::none();
            }
        };
        let id = self.jobs.len();
        self.jobs.push(Job::new(format!("{} {}", verb.word(), key.label()), &step, &key.manager, &key.name));
        jobs::start(id, step)
    }

    /// When uninstalling `key` would uninstall a manager (pipx is an apt
    /// package), say so: its own packages would be orphaned.
    fn manager_warning(&self, key: &Key) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        let name = key.name.trim_start_matches("python3-");
        let d = snapshot.detections.iter().find(|d| d.recipe.name == dupe_key(name))?;
        let installed_through_this =
            matches!(&d.status, Status::Installed { via: Via::Manager(m), .. } if *m == key.manager);
        installed_through_this.then(|| {
            format!(
                "This is how {0} was installed. Its own packages would be left behind; `pmm remove {0}` removes them too and saves a list first.",
                d.recipe.name
            )
        })
    }

    /// Ask every repository (or only the current page's manager) about
    /// the search box's text, in the background; answers stream in.
    fn search_repositories(&mut self) -> Task<Message> {
        let query = self.query.trim().to_string();
        let Some(snapshot) = self.snapshot.clone() else { return Task::none() };
        if query.is_empty() {
            return Task::none();
        }
        let only = match self.page() {
            Page::Manager(m) => Some(m),
            _ => None,
        };
        let detections: Vec<_> = remote::searchable(&snapshot.detections)
            .into_iter()
            .filter(|d| only.as_ref().is_none_or(|m| &d.recipe.name == m))
            .cloned()
            .collect();
        let id = self.remote.as_ref().map_or(0, |r| r.id + 1);
        self.tab = Tab::Repositories;
        self.remote = Some(RemoteSearch {
            id,
            query: query.clone(),
            results: Vec::new(),
            waiting: detections.iter().map(|d| d.recipe.name.clone()).collect(),
            expanded: Vec::new(),
        });
        if detections.is_empty() {
            return Task::none();
        }
        let (tx, rx) = cosmic::iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            // Installed marks come from the snapshot instead (see `is_installed`).
            let _ = remote::search_remote(&detections, &[], &query, |results| {
                let _ = tx.unbounded_send(results);
            });
        });
        use cosmic::iced::futures::StreamExt;
        cosmic::task::stream(rx.map(move |results| Message::RemoteAnswer(id, Box::new(results))))
    }

    /// Whether `manager` has `name` installed, by the duplicate rules.
    fn is_installed(&self, manager: &str, name: &str) -> bool {
        let key = dupe_key(name);
        self.snapshot
            .as_ref()
            .is_some_and(|s| s.rows.iter().any(|r| r.manager == manager && dupe_key(&r.package.name) == key))
    }

    /// A job for `name` with `manager` that hasn't finished.
    fn busy(&self, manager: &str, name: &str) -> bool {
        self.jobs.iter().any(|j| j.state == State::Running && j.manager == manager && j.package == name)
    }

    fn repositories<'a>(&'a self, snapshot: &'a Snapshot) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();
        let Some(search) = &self.remote else {
            return widget::container(widget::text::body("Press Enter in the search box to look in every manager's repository."))
                .center(Length::Fill)
                .into();
        };
        let mut list = widget::Column::new().spacing(spacing.space_s);
        let status = match search.waiting.as_slice() {
            [] => format!("Results for “{}”", search.query),
            waiting => format!("Results for “{}” · waiting for {}…", search.query, waiting.join(", ")),
        };
        list = list.push(widget::text::caption(status));

        let mut empty = Vec::new();
        for results in &search.results {
            let found = match &results.found {
                Ok(found) if found.is_empty() => {
                    empty.push(if results.exact_only {
                        format!("{} (exact names only)", results.manager)
                    } else {
                        results.manager.clone()
                    });
                    continue;
                }
                Ok(found) => found,
                Err(e) => {
                    list = list.push(widget::warning(format!("Couldn't search {}: {e}", results.manager)));
                    continue;
                }
            };
            let recipe = snapshot.detection(&results.manager).map(|d| &d.recipe);
            let can_install = recipe.is_some_and(|r| r.install_pkg.is_some());
            let all = search.expanded.contains(&results.manager);
            let shown = if all { found.len() } else { found.len().min(REMOTE_SHOWN) };

            let mut group = widget::Column::new()
                .push(widget::text::heading(format!("{}  ·  {:.1}s", results.manager, results.took.as_secs_f32())))
                .spacing(spacing.space_xxs);
            for f in &found[..shown] {
                let p = &f.package;
                let installed = self.is_installed(&results.manager, &p.name);
                let action: Element<'a, Message> = if installed {
                    widget::text::caption("Installed").class(cosmic::theme::Text::Accent).into()
                } else if self.busy(&results.manager, &p.name) {
                    widget::text::caption("Installing…").into()
                } else if can_install {
                    widget::button::standard("Install")
                        .on_press(Message::Run(
                            Verb::Install,
                            Key { manager: results.manager.clone(), name: p.name.clone(), version: None },
                        ))
                        .into()
                } else {
                    widget::text::caption(format!("{} can't install here", results.manager)).into()
                };
                let about = [p.title.as_deref(), p.note.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
                let name = widget::text::body(p.name.as_str()).wrapping(Wrapping::None);
                let name: Element<'a, Message> = if f.rank == Rank::Exact {
                    name.font(cosmic::font::bold()).into()
                } else {
                    name.into()
                };
                let cell = |content: Element<'a, Message>, portion: u16| {
                    widget::container(content).width(Length::FillPortion(portion)).clip(true)
                };
                group = group.push(
                    widget::Row::new()
                        .push(cell(name, 5))
                        .push(cell(
                            widget::text::body(clip(p.version.as_deref().unwrap_or(""), 20)).wrapping(Wrapping::None).into(),
                            3,
                        ))
                        .push(cell(widget::text::caption(about).wrapping(Wrapping::None).into(), 10))
                        .push(widget::container(action).width(Length::Fixed(140.0)).align_x(Alignment::End))
                        .spacing(spacing.space_s)
                        .align_y(Alignment::Center),
                );
            }
            if shown < found.len() {
                group = group.push(
                    widget::button::text(format!("Show all {}", found.len())).on_press(Message::ShowAll(results.manager.clone())),
                );
            }
            list = list.push(widget::container(group).padding(spacing.space_s).class(cosmic::theme::Container::Card).width(Length::Fill));
        }
        if !empty.is_empty() {
            list = list.push(widget::text::caption(format!("No results: {}", empty.join(", "))));
        }
        widget::scrollable(list).height(Length::Fill).into()
    }

    fn header_cell(&self, label: &str, column: Column, width: Length) -> Element<'_, Message> {
        let arrow = match (self.sort == column, self.ascending) {
            (false, _) => "",
            (true, true) => " ▲",
            (true, false) => " ▼",
        };
        // Buttons size to their label; the container gives the column its
        // share of the width, the same as the cells below.
        // A text button whose label never wraps (the arrow used to drop onto
        // a second line in narrow columns).
        let label = widget::text::body(format!("{label}{arrow}")).wrapping(Wrapping::None);
        let spacing = cosmic::theme::spacing();
        widget::container(
            widget::button::custom(label)
                .class(cosmic::theme::Button::Text)
                .padding([spacing.space_xxs, spacing.space_s])
                .on_press(Message::Sort(column)),
        )
        .width(width)
        .clip(true)
        .into()
    }

    fn table<'a>(&'a self, snapshot: &'a Snapshot) -> Element<'a, Message> {
        let rows: Vec<&Row> = self.visible.iter().filter_map(|&i| snapshot.rows.get(i)).collect();
        let spacing = cosmic::theme::spacing();

        let header = widget::Row::new()
            .push(self.header_cell("Name", Column::Name, Length::FillPortion(NAME)))
            .push(self.header_cell("Version", Column::Version, Length::FillPortion(VERSION)))
            .push(self.header_cell("Manager", Column::Manager, Length::FillPortion(MANAGER)))
            .push(
                widget::container(widget::text::heading("Notes"))
                    .padding([0, spacing.space_s])
                    .width(Length::FillPortion(NOTES)),
            )
            .spacing(spacing.space_s)
            .align_y(Alignment::Center);

        let mut list = widget::Column::new().spacing(spacing.space_xxxs);
        for row in rows.iter().take(SHOWN) {
            let mut notes = widget::Column::new().push(widget::text::caption(row.extra()));
            if !row.also.is_empty() {
                notes = notes.push(
                    widget::text::caption(format!("also via {}", row.also.join(", ")))
                        .class(cosmic::theme::Text::Accent),
                );
            }
            // Text that doesn't fit is cut off at the cell's edge rather than
            // wrapping or spilling into the next column. Cells are inset like
            // the header buttons' labels, so they line up.
            let cell = |content: Element<'a, Message>, portion: u16| {
                widget::container(content)
                    .padding([0, spacing.space_s])
                    .width(Length::FillPortion(portion))
                    .clip(true)
            };
            let one_line = |text: String| widget::text::body(text).wrapping(Wrapping::None).into();
            let line = widget::Row::new()
                .push(cell(one_line(row.package.name.clone()), NAME))
                .push(cell(one_line(clip(row.package.version.as_deref().unwrap_or(""), 30)), VERSION))
                .push(cell(one_line(row.manager.clone()), MANAGER))
                .push(cell(notes.into(), NOTES))
                .spacing(spacing.space_s)
                .align_y(Alignment::Center);
            list = list.push(
                widget::button::custom(line)
                    .class(cosmic::theme::Button::MenuItem)
                    .padding([spacing.space_xxxs, 0])
                    .width(Length::Fill)
                    .on_press(Message::Open(Key::of(row))),
            );
        }

        let summary = match (rows.len(), self.query.trim().is_empty()) {
            (0, false) => format!("Nothing installed matches “{}”", self.query.trim()),
            (0, true) => "Nothing here".to_string(),
            (n, _) if n > SHOWN => format!("{n} packages · showing the first {SHOWN}; search to narrow it down"),
            (1, _) => "1 package".to_string(),
            (n, _) => format!("{n} packages"),
        };

        widget::Column::new()
            .push(widget::text::caption(summary))
            .push(header)
            .push(widget::divider::horizontal::default())
            .push(widget::scrollable(list).height(Length::Fill))
            .spacing(spacing.space_xs)
            .into()
    }

    fn job_view<'a>(&'a self, job: &'a Job, lines: usize) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();
        let (status, class) = match &job.state {
            State::Running => ("Running…".to_string(), cosmic::theme::Text::Default),
            State::Done => ("Done".to_string(), cosmic::theme::Text::Accent),
            State::Failed(e) => (format!("Failed: {e}"), cosmic::theme::Text::Default),
        };
        let mut output = widget::Column::new();
        for line in &job.output[job.output.len().saturating_sub(lines)..] {
            output = output.push(widget::text::monotext(line.as_str()).wrapping(Wrapping::None));
        }
        let mut column = widget::Column::new()
            .push(widget::text::heading(job.title.as_str()))
            .push(widget::text::caption(format!("$ {}", job.command)))
            .push(widget::text::body(status).class(class))
            .spacing(spacing.space_xxs);
        if !job.output.is_empty() {
            // Long lines are cut at the edge, like a terminal without wrapping.
            column = column.push(widget::container(output).width(Length::Fill).clip(true));
        }
        widget::container(column).padding(spacing.space_s).class(cosmic::theme::Container::Card).width(Length::Fill).into()
    }

    fn activity(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        if self.jobs.is_empty() {
            return widget::container(widget::text::body("Nothing run yet. Upgrades and uninstalls show up here, with their output."))
                .center(Length::Fill)
                .into();
        }
        let mut list = widget::Column::new().spacing(spacing.space_s);
        for job in self.jobs.iter().rev() {
            list = list.push(self.job_view(job, 200));
        }
        widget::scrollable(list).height(Length::Fill).into()
    }

    fn package_page<'a>(&'a self, snapshot: &'a Snapshot, key: &'a Key) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();
        let mut page = widget::Column::new().spacing(spacing.space_m);

        // Every copy, with what can be done to it.
        let copies = self.copies(snapshot, key);
        if copies.is_empty() {
            page = page.push(widget::text::body("No longer installed."));
        }
        if copies.len() > 1 {
            page = page.push(widget::text::body(format!("Installed {} times:", copies.len())));
        }
        for row in &copies {
            let copy = Key::of(row);
            let recipe = snapshot.detection(&row.manager).map(|d| &d.recipe);
            let busy = self.busy(&copy.manager, &copy.name);
            let button = |verb: Verb, destructive: bool| {
                let available = recipe.and_then(|r| verb.action(r)).is_some();
                let label = verb.word();
                let b = if destructive { widget::button::destructive(label) } else { widget::button::standard(label) };
                b.on_press_maybe((available && !busy).then(|| Message::Run(verb, copy.clone())))
            };
            let title = format!("{} {}", row.manager, row.package.version.as_deref().unwrap_or(""));
            let mut card = widget::Column::new()
                .push(widget::text::heading(title))
                .spacing(spacing.space_xs);
            let extra = row.extra();
            if !extra.is_empty() {
                card = card.push(widget::text::caption(extra));
            }
            card = card.push(
                widget::Row::new()
                    .push(button(Verb::Upgrade, false))
                    .push(button(Verb::Uninstall, true))
                    .spacing(spacing.space_xs),
            );
            if let Some(job) = self.jobs.iter().rev().find(|j| j.manager == copy.manager && j.package == copy.name) {
                card = card.push(self.job_view(job, 8));
            }
            page = page.push(widget::container(card).padding(spacing.space_s).class(cosmic::theme::Container::Card).width(Length::Fill));
        }

        // What the manager says about it.
        let details: Element<'a, Message> = match self.info.get(key) {
            None | Some(None) => widget::text::caption("Asking for details…").into(),
            Some(Some(Err(e))) => widget::text::caption(format!("No details: {e}")).into(),
            Some(Some(Ok(text))) => {
                let mut lines = widget::Column::new();
                for line in text.lines().take(INFO_LINES) {
                    lines = lines.push(widget::text::caption(line));
                }
                lines.into()
            }
        };
        page.push(
            widget::Column::new()
                .push(widget::text::heading(format!("From {}", key.manager)))
                .push(details)
                .spacing(spacing.space_xs),
        )
        .into()
    }
}

/// Long versions (Pop!_OS builds like `5.3.0-0pop1~1710896694~24.04~213b5ed`)
/// are cut short; the cell clips the rest.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

impl cosmic::Application for App {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    // Placeholder until the project has a home on GitHub.
    const APP_ID: &'static str = "io.github.pmm.Pmm";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
        let mut app = App {
            core,
            nav: nav_bar::Model::default(),
            snapshot: None,
            loading: false,
            query: String::new(),
            sort: Column::Name,
            ascending: true,
            selected: None,
            info: HashMap::new(),
            confirm: None,
            jobs: Vec::new(),
            visible: Vec::new(),
            tab: Tab::Installed,
            remote: None,
        };
        app.set_header_title("Package Manager Manager".into());
        let task = app.load(false);
        (app, task)
    }

    fn nav_model(&self) -> Option<&nav_bar::Model> {
        Some(&self.nav)
    }

    fn on_nav_select(&mut self, id: nav_bar::Id) -> Task<Message> {
        self.nav.activate(id);
        self.refilter();
        Task::none()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Loaded(snapshot) => {
                self.loading = false;
                self.fill_nav(&snapshot);
                self.snapshot = Some(snapshot);
                self.refilter();
                // Details may have changed with the packages.
                self.info.clear();
                if let Some(key) = self.selected.clone() {
                    return self.open(key);
                }
            }
            Message::Search(query) => {
                self.query = query;
                self.refilter();
            }
            Message::ClearSearch => {
                self.query.clear();
                self.refilter();
            }
            Message::Refresh => {
                if !self.loading {
                    return self.load(true);
                }
            }
            Message::Sort(column) => {
                self.ascending = self.sort != column || !self.ascending;
                self.sort = column;
                self.refilter();
            }
            Message::Open(key) => return self.open(key),
            Message::CloseDrawer => {
                self.selected = None;
                self.set_show_context(false);
            }
            Message::InfoLoaded(key, answer) => {
                self.info.insert(key, Some(answer));
            }
            Message::Run(verb @ (Verb::Install | Verb::Upgrade), key) => return self.start(verb, &key),
            Message::Run(Verb::Uninstall, key) => {
                // Ask first; meanwhile find out what else it would remove.
                let manager_warning = self.manager_warning(&key);
                self.confirm = Some(Confirm { key: key.clone(), preview: Preview::Checking, manager_warning });
                let action = self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.detection(&key.manager))
                    .and_then(|d| d.recipe.remove_pkg.clone());
                return cosmic::task::future(async move {
                    let (name, version) = (key.name.clone(), key.version.clone());
                    let preview = off_thread(move || match action {
                        Some(action) => action::preview(&action, &name, version.as_deref()),
                        None => Ok(None),
                    })
                    .await
                    .unwrap_or(Err("pmm crashed while checking".into()));
                    Message::PreviewLoaded(key, preview)
                });
            }
            Message::PreviewLoaded(key, preview) => {
                if let Some(confirm) = self.confirm.as_mut().filter(|c| c.key == key) {
                    confirm.preview = match preview {
                        Ok(names) => Preview::Ready(names),
                        Err(e) => Preview::Refused(e),
                    };
                }
            }
            Message::Confirmed => {
                if let Some(confirm) = self.confirm.take() {
                    return self.start(Verb::Uninstall, &confirm.key);
                }
            }
            Message::Cancelled => self.confirm = None,
            Message::ShowTab(tab) => self.tab = tab,
            Message::SearchRepositories => return self.search_repositories(),
            Message::RemoteAnswer(id, results) => {
                if let Some(search) = self.remote.as_mut().filter(|r| r.id == id) {
                    search.waiting.retain(|m| *m != results.manager);
                    search.results.push(*results);
                }
            }
            Message::ShowAll(manager) => {
                if let Some(search) = self.remote.as_mut() {
                    search.expanded.push(manager);
                }
            }
            Message::Job(id, event) => {
                let finished_ok = matches!(event, JobEvent::Finished(Ok(())));
                if let Some(job) = self.jobs.get_mut(id) {
                    job.apply(event);
                }
                if finished_ok {
                    // Re-read; the cache notices the change by itself.
                    return self.load(false);
                }
            }
        }
        Task::none()
    }

    fn header_end(&self) -> Vec<Element<'_, Message>> {
        let refresh = widget::button::icon(widget::icon::from_name("view-refresh-symbolic"))
            .on_press_maybe((!self.loading).then_some(Message::Refresh));
        vec![widget::tooltip(refresh, widget::text("Re-read every manager"), widget::tooltip::Position::Bottom).into()]
    }

    fn context_drawer(&self) -> Option<ContextDrawer<'_, Message>> {
        let key = self.selected.as_ref()?;
        let snapshot = self.snapshot.as_ref()?;
        let content = widget::scrollable(self.package_page(snapshot, key));
        Some(context_drawer::context_drawer(content, Message::CloseDrawer).title(key.name.as_str()))
    }

    fn dialog(&self) -> Option<Element<'_, Message>> {
        let confirm = self.confirm.as_ref()?;
        let key = &confirm.key;
        let (body, ready) = match &confirm.preview {
            Preview::Checking => (format!("Checking what else {} would remove…", key.manager), false),
            Preview::Refused(why) => (format!("{} won't remove it:\n\n{why}", key.manager), false),
            Preview::Ready(None) => (format!("pmm will run {}'s uninstall for it.", key.manager), true),
            Preview::Ready(Some(names)) => {
                let others: Vec<&str> = names.iter().map(String::as_str).filter(|n| *n != key.name).collect();
                match others.as_slice() {
                    [] => ("Nothing else will be removed.".to_string(), true),
                    others => (format!("This also removes {}: {}", others.len(), others.join(", ")), true),
                }
            }
        };
        let body = match &confirm.manager_warning {
            Some(warning) => format!("{body}\n\n⚠ {warning}"),
            None => body,
        };
        let uninstall = widget::button::destructive("Uninstall").on_press_maybe(ready.then_some(Message::Confirmed));
        Some(
            widget::dialog()
                .title(format!("Uninstall {}?", key.label()))
                .body(body)
                .primary_action(uninstall)
                .secondary_action(widget::button::standard("Cancel").on_press(Message::Cancelled))
                .into(),
        )
    }

    fn view(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let Some(snapshot) = &self.snapshot else {
            return widget::container(widget::text::title3("Reading your package managers…"))
                .center(Length::Fill)
                .into();
        };
        if self.page() == Page::Activity {
            return widget::container(self.activity()).padding(spacing.space_s).into();
        }

        let search = widget::text_input::search_input("Search packages · Enter also searches repositories", &self.query)
            .on_input(Message::Search)
            .on_clear(Message::ClearSearch)
            .on_submit(|_| Message::SearchRepositories);
        let tab = |label: String, tab: Tab| {
            let button = if self.tab == tab { widget::button::suggested(label) } else { widget::button::standard(label) };
            button.on_press(Message::ShowTab(tab))
        };
        let remote_label = match &self.remote {
            Some(r) if !r.waiting.is_empty() => "Repositories (searching…)".to_string(),
            Some(r) => {
                let n: usize = r.results.iter().filter_map(|x| x.found.as_ref().ok()).map(Vec::len).sum();
                format!("Repositories ({n})")
            }
            None => "Repositories".to_string(),
        };
        let mut tabs = widget::Row::new()
            .push(tab(format!("Installed ({})", self.visible.len()), Tab::Installed))
            .push(tab(remote_label, Tab::Repositories))
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center);
        let query = self.query.trim();
        if !query.is_empty() && self.remote.as_ref().is_none_or(|r| r.query != query) {
            tabs = tabs.push(widget::button::text(format!("Search repositories for “{query}”")).on_press(Message::SearchRepositories));
        }
        let mut content = widget::Column::new().push(search).push(tabs).spacing(spacing.space_s);
        if self.loading {
            content = content.push(widget::text::caption("Re-reading every manager…"));
        }
        for problem in &snapshot.problems {
            content = content.push(widget::warning(problem.as_str()));
        }
        let running = self.jobs.iter().filter(|j| j.state == State::Running).count();
        if running > 0 {
            content = content.push(widget::text::caption(format!("{running} running; see Activity")));
        }
        let body = match self.tab {
            Tab::Installed => self.table(snapshot),
            Tab::Repositories => self.repositories(snapshot),
        };
        content.push(body).padding(spacing.space_s).into()
    }
}
