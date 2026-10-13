//! Commands the GUI runs (installs, uninstalls, upgrades), shown live on the
//! Activity page. Each runs on its own thread; its output comes back as a
//! stream of messages.

use cosmic::app::Task;
use cosmic::iced::futures::StreamExt;
use cosmic::iced::futures::channel::mpsc;
use pmm_core::action::{self, Event, Io, Step};

use crate::app::Message;

/// Root via the graphical password prompt: there's no terminal for sudo.
pub const ELEVATE: &str = "pkexec";

/// Lines kept per job; apt can print thousands.
const KEEP: usize = 500;

#[derive(Debug, Clone)]
pub enum JobEvent {
    Line(String),
    Finished(Result<(), String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Running,
    Done,
    Failed(String),
}

/// One command run, identified by its place in the app's list of jobs.
pub struct Job {
    /// "Uninstall pipx (apt)".
    pub title: String,
    pub command: String,
    pub output: Vec<String>,
    pub state: State,
    /// The package it acts on, so its page can show progress.
    pub manager: String,
    pub package: String,
}

impl Job {
    pub fn new(title: String, step: &Step, manager: &str, package: &str) -> Job {
        Job {
            title,
            command: step.shown(ELEVATE),
            output: Vec::new(),
            state: State::Running,
            manager: manager.into(),
            package: package.into(),
        }
    }

    pub fn apply(&mut self, event: JobEvent) {
        match event {
            JobEvent::Line(line) => {
                if self.output.len() == KEEP {
                    self.output.remove(0);
                }
                self.output.push(line);
            }
            JobEvent::Finished(Ok(())) => self.state = State::Done,
            JobEvent::Finished(Err(e)) => self.state = State::Failed(e),
        }
    }
}

/// Run `step` in the background, reporting as `Message::Job(id, …)`.
pub fn start(id: usize, step: Step) -> Task<Message> {
    let (tx, rx) = mpsc::unbounded();
    std::thread::spawn(move || {
        let result = action::run(std::slice::from_ref(&step), ELEVATE, Io::Stream, |event| {
            if let Event::Output(line) = event {
                let _ = tx.unbounded_send(JobEvent::Line(line.to_string()));
            }
        });
        let result = result.map(|_| ()).map_err(|f| match f.reason.as_str() {
            // pkexec's exit codes when the password prompt is dismissed or fails.
            "exited with code 126" | "exited with code 127" if step.sudo => {
                "the password prompt was cancelled or failed".to_string()
            }
            _ => f.reason,
        });
        let _ = tx.unbounded_send(JobEvent::Finished(result));
    });
    cosmic::task::stream(rx.map(move |event| Message::Job(id, event)))
}
