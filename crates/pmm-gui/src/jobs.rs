//! Commands the GUI runs (installs, uninstalls, upgrades, updates), shown
//! live on the Activity page. They run in order on a background thread;
//! their output comes back as a stream of messages.

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
    /// Its turn came (jobs in a batch wait for the ones before).
    Started,
    Line(String),
    Finished(Result<(), String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Queued,
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
    /// The package it acts on, so its page can show progress; empty for a
    /// whole manager's update.
    pub manager: String,
    pub package: String,
}

impl Job {
    pub fn new(title: String, steps: &[Step], manager: &str, package: &str) -> Job {
        Job {
            title,
            command: steps.iter().map(|s| s.shown(ELEVATE)).collect::<Vec<_>>().join(" && "),
            output: Vec::new(),
            state: State::Queued,
            manager: manager.into(),
            package: package.into(),
        }
    }

    pub fn apply(&mut self, event: JobEvent) {
        match event {
            JobEvent::Started => self.state = State::Running,
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

/// Jobs run one at a time, in the order queued, on one background thread:
/// two at once could fight over the same lock (apt's) or files, and would
/// each ask for the password at the same time. A failed job doesn't stop
/// the ones after it.
#[derive(Default)]
pub struct Queue {
    tx: Option<std::sync::mpsc::Sender<(usize, Vec<Step>)>>,
}

impl Queue {
    /// Queue job `id`; it reports as `Message::Job(id, …)`. Returns the task
    /// that carries every job's events, the first time (or after the thread
    /// stopped); later jobs reuse it.
    pub fn push(&mut self, id: usize, steps: Vec<Step>) -> Task<Message> {
        let job = match self.tx.as_ref().map(|tx| tx.send((id, steps.clone()))) {
            Some(Ok(())) => return Task::none(),
            _ => (id, steps),
        };
        let (tx, jobs) = std::sync::mpsc::channel();
        let _ = tx.send(job);
        self.tx = Some(tx);
        let (events, rx) = mpsc::unbounded();
        std::thread::spawn(move || {
            for (id, steps) in jobs {
                let send = |event| events.unbounded_send((id, event));
                let _ = send(JobEvent::Started);
                let result = action::run(&action::join_sudo(&steps), ELEVATE, Io::Stream, |event| {
                    if let Event::Output(line) = event {
                        let _ = send(JobEvent::Line(line.to_string()));
                    }
                });
                let result = result.map(|_| ()).map_err(|f| match f.reason.as_str() {
                    // pkexec's exit codes when the password prompt is dismissed or fails.
                    "exited with code 126" | "exited with code 127" if f.step.sudo => {
                        "the password prompt was cancelled or failed".to_string()
                    }
                    _ => f.reason,
                });
                let _ = send(JobEvent::Finished(result));
            }
        });
        cosmic::task::stream(rx.map(|(id, event)| Message::Job(id, event)))
    }
}
