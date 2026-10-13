//! pmm-gui: the package manager manager as a COSMIC desktop app.

mod app;
mod data;

fn main() -> cosmic::iced::Result {
    let settings = cosmic::app::Settings::default()
        .size(cosmic::iced::Size::new(1100.0, 760.0))
        // libcosmic defaults to a transparent window, which showed whatever
        // was behind it through the table even with an opaque theme.
        .transparent(false);
    cosmic::app::run::<app::App>(settings, ())
}
