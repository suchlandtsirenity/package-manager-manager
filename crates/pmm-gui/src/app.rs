//! The Installed view: a sidebar of managers and one searchable, sortable
//! table of everything they've installed, with duplicates flagged.

use std::cmp::Ordering;
use std::sync::Arc;

use cosmic::app::{Core, Task};
use cosmic::iced::futures::channel::oneshot;
use cosmic::iced::{Alignment, Length};
use cosmic::prelude::*;
use cosmic::widget::{self, nav_bar};
use pmm_core::Rank;
use pmm_core::inventory::rank;

use crate::data::{self, Row, Snapshot};

/// Rows drawn at once. Thousands of rows make every redraw slow, and nobody
/// scrolls through 3000 packages: they search.
const SHOWN: usize = 400;

/// What a sidebar entry shows.
#[derive(Debug, Clone, PartialEq)]
enum Page {
    All,
    Duplicates,
    Manager(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Name,
    Version,
    Manager,
}

#[derive(Debug, Clone)]
pub enum Message {
    Loaded(Arc<Snapshot>),
    Search(String),
    ClearSearch,
    Refresh,
    Sort(Column),
}

pub struct App {
    core: Core,
    nav: nav_bar::Model,
    snapshot: Option<Arc<Snapshot>>,
    loading: bool,
    query: String,
    sort: Column,
    ascending: bool,
}

impl App {
    /// Read everything on a separate thread; the list commands take up to a
    /// second, which would freeze the window.
    fn load(&mut self, refresh: bool) -> Task<Message> {
        self.loading = true;
        cosmic::task::future(async move {
            let (tx, rx) = oneshot::channel();
            std::thread::spawn(move || {
                let _ = tx.send(data::load(refresh));
            });
            Message::Loaded(Arc::new(rx.await.unwrap_or_default()))
        })
    }

    /// Rebuild the sidebar from a new snapshot, keeping the selected page.
    fn fill_nav(&mut self, snapshot: &Snapshot) {
        let selected = self.nav.active_data::<Page>().cloned().unwrap_or(Page::All);
        self.nav.clear();
        let all = self.nav.insert().text(format!("All packages  {}", snapshot.rows.len())).data(Page::All).id();
        let dupes = snapshot.duplicate_packages;
        self.nav.insert().text(format!("Duplicates  {dupes}")).data(Page::Duplicates);
        let mut first = true;
        for m in &snapshot.managers {
            let count = m.count.map_or("?".to_string(), |n| n.to_string());
            self.nav
                .insert()
                .text(format!("{}  {count}", m.name))
                .data(Page::Manager(m.name.clone()))
                .divider_above(std::mem::take(&mut first));
        }
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

    /// The rows for the current page and search, best matches first when
    /// searching, otherwise in the chosen column order.
    fn rows<'a>(&self, snapshot: &'a Snapshot) -> Vec<&'a Row> {
        let page = self.page();
        let query = self.query.trim().to_lowercase();
        let mut rows: Vec<(&Row, Option<Rank>)> = snapshot
            .rows
            .iter()
            .filter(|r| match &page {
                Page::All => true,
                Page::Duplicates => !r.also.is_empty(),
                Page::Manager(m) => &r.manager == m,
            })
            .filter_map(|r| match query.is_empty() {
                true => Some((r, None)),
                false => rank(&r.package, &query).map(|k| (r, Some(k))),
            })
            .collect();
        rows.sort_by(|(a, ra), (b, rb)| ra.cmp(rb).then_with(|| self.compare(a, b)));
        rows.into_iter().map(|(r, _)| r).collect()
    }

    fn compare(&self, a: &Row, b: &Row) -> Ordering {
        let by_name = |a: &Row, b: &Row| a.package.name.to_lowercase().cmp(&b.package.name.to_lowercase());
        let order = match self.sort {
            Column::Name => by_name(a, b).then_with(|| a.manager.cmp(&b.manager)),
            Column::Version => a.package.version.cmp(&b.package.version).then_with(|| by_name(a, b)),
            Column::Manager => a.manager.cmp(&b.manager).then_with(|| by_name(a, b)),
        };
        if self.ascending { order } else { order.reverse() }
    }

    fn header_cell(&self, label: &str, column: Column, width: Length) -> Element<'_, Message> {
        let arrow = match (self.sort == column, self.ascending) {
            (false, _) => "",
            (true, true) => " ▲",
            (true, false) => " ▼",
        };
        // Buttons size to their label; the container gives the column its
        // share of the width, the same as the cells below.
        widget::container(widget::button::text(format!("{label}{arrow}")).on_press(Message::Sort(column)))
            .width(width)
            .into()
    }

    fn table<'a>(&'a self, snapshot: &'a Snapshot) -> Element<'a, Message> {
        let rows = self.rows(snapshot);
        let spacing = cosmic::theme::spacing();

        let header = widget::Row::new()
            .push(self.header_cell("Name", Column::Name, Length::FillPortion(4)))
            .push(self.header_cell("Version", Column::Version, Length::FillPortion(2)))
            .push(self.header_cell("Manager", Column::Manager, Length::FillPortion(1)))
            .push(
                widget::container(widget::text::heading("Notes"))
                    .padding([0, spacing.space_s])
                    .width(Length::FillPortion(3)),
            )
            .spacing(spacing.space_s)
            .align_y(Alignment::Center);

        let mut list = widget::Column::new().spacing(spacing.space_xxs);
        for row in rows.iter().take(SHOWN) {
            let mut notes = widget::Column::new().push(widget::text::caption(row.extra()));
            if !row.also.is_empty() {
                notes = notes.push(
                    widget::text::caption(format!("also via {}", row.also.join(", ")))
                        .class(cosmic::theme::Text::Accent),
                );
            }
            // Each cell is inset like the header buttons' labels, so they line up.
            let cell = |content: Element<'a, Message>, portion: u16| {
                widget::container(content).padding([0, spacing.space_s]).width(Length::FillPortion(portion))
            };
            list = list.push(
                widget::Row::new()
                    .push(cell(widget::text::body(row.package.name.as_str()).into(), 4))
                    .push(cell(widget::text::body(clip(row.package.version.as_deref().unwrap_or(""), 22)).into(), 2))
                    .push(cell(widget::text::body(row.manager.as_str()).into(), 1))
                    .push(cell(notes.into(), 3))
                    .spacing(spacing.space_s),
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
}

/// Long versions (Pop!_OS builds like `5.3.0-0pop1~1710896694~24.04~213b5ed`)
/// would spill into the next column.
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
        Task::none()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Loaded(snapshot) => {
                self.loading = false;
                self.fill_nav(&snapshot);
                self.snapshot = Some(snapshot);
            }
            Message::Search(query) => self.query = query,
            Message::ClearSearch => self.query.clear(),
            Message::Refresh => {
                if !self.loading {
                    return self.load(true);
                }
            }
            Message::Sort(column) => {
                self.ascending = self.sort != column || !self.ascending;
                self.sort = column;
            }
        }
        Task::none()
    }

    fn header_end(&self) -> Vec<Element<'_, Message>> {
        let refresh = widget::button::icon(widget::icon::from_name("view-refresh-symbolic"))
            .on_press_maybe((!self.loading).then_some(Message::Refresh));
        vec![widget::tooltip(refresh, widget::text("Re-read every manager"), widget::tooltip::Position::Bottom).into()]
    }

    fn view(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let Some(snapshot) = &self.snapshot else {
            return widget::container(widget::text::title3("Reading your package managers…"))
                .center(Length::Fill)
                .into();
        };

        let search = widget::text_input::search_input("Search installed packages", &self.query)
            .on_input(Message::Search)
            .on_clear(Message::ClearSearch);
        let mut content = widget::Column::new().push(search).spacing(spacing.space_s);
        if self.loading {
            content = content.push(widget::text::caption("Re-reading every manager…"));
        }
        for problem in &snapshot.problems {
            content = content.push(widget::warning(problem.as_str()));
        }
        content
            .push(self.table(snapshot))
            .padding(spacing.space_s)
            .into()
    }
}
