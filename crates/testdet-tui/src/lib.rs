//! Terminal UI frontend for TestDet, built on ratatui and crossterm.
//!
//! The crate owns rendering, key handling and the event loop, but not the
//! network: a [`Driver`] supplied by the caller starts and cancels runs and
//! hands their [`Event`]s back through a non-blocking [`Driver::events`] drain.
//! That keeps a unit test free of a terminal and the real executor free to
//! live in `testdet-cli`, which is the only crate allowed to depend on tokio
//! and reqwest.

mod app;
mod keys;
mod request_pane;
#[cfg(test)]
mod test_support;
mod tree;

use std::error::Error;
use std::fmt;
use std::io;
use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use testdet_core::Event;

pub use crate::app::{Action, App};

/// How long the loop waits for a key before draining the driver's events.
const POLL: Duration = Duration::from_millis(50);

/// The executor the UI drives: start a run, cancel it, and hand back whatever
/// the run produced since the last call.
///
/// [`Driver::events`] must never block — it is the poll loop's only way to
/// keep the frame responsive while a request is in flight.
pub trait Driver {
    /// Start `operation`, as `user` (`None` is anonymous).
    fn start(&mut self, operation: &str, user: Option<&str>);
    /// Cancel the in-flight run, if any.
    fn cancel(&mut self);
    /// Take every event received since the last call, without blocking.
    fn events(&mut self) -> Vec<Event>;
}

/// Anything that can go wrong bringing the terminal up or down.
#[derive(Debug)]
pub enum TuiError {
    /// A terminal operation failed.
    Io(io::Error),
}

impl fmt::Display for TuiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "terminal error: {error}"),
        }
    }
}

impl Error for TuiError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
        }
    }
}

impl From<io::Error> for TuiError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Owns raw mode and the alternate screen, and gives both back on drop so an
/// error can never leave the user's terminal in the TUI's state.
struct TerminalGuard {
    restored: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self, TuiError> {
        let mut guard = Self { restored: false };
        if let Err(error) = enable_raw_mode()
            .map_err(TuiError::Io)
            .and_then(|()| execute!(io::stdout(), EnterAlternateScreen).map_err(TuiError::Io))
        {
            guard.restore();
            return Err(error);
        }
        Ok(guard)
    }

    fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Run the UI until the user quits.
///
/// The loop polls for a key for at most `POLL`, handles it, then drains the
/// driver; it never waits on the network and never blocks on a future.
pub fn run(mut app: App, driver: &mut dyn Driver) -> Result<(), TuiError> {
    let mut guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let outcome = event_loop(&mut terminal, &mut app, driver);
    guard.restore();
    outcome
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    driver: &mut dyn Driver,
) -> Result<(), TuiError> {
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(POLL)?
            && let CrosstermEvent::Key(key) = event::read()?
        {
            let action = app.handle_key(key);
            if !dispatch(app, action, driver)? {
                return Ok(());
            }
        }
        for event in driver.events() {
            app.apply_event(event);
        }
    }
}

/// Apply one action to the driver. Returns `false` when the UI should stop.
///
/// Kept separate from the terminal so it can be tested with a fake driver.
fn dispatch(app: &App, action: Action, driver: &mut dyn Driver) -> Result<bool, TuiError> {
    match action {
        Action::Quit => return Ok(false),
        Action::Run(operation) => driver.start(&operation, app.user()),
        Action::Cancel => driver.cancel(),
        // Switching environment needs the config re-resolved, which only the
        // host can do; the CLI reads `app.environment()` between frames.
        Action::None
        | Action::SwitchUser(_)
        | Action::SwitchEnv(_)
        | Action::StoreVar
        | Action::Filter(_) => {}
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeDriver {
        started: Vec<(String, Option<String>)>,
        cancelled: usize,
        pending: Vec<Event>,
    }

    impl Driver for FakeDriver {
        fn start(&mut self, operation: &str, user: Option<&str>) {
            self.started
                .push((operation.to_string(), user.map(str::to_string)));
        }

        fn cancel(&mut self) {
            self.cancelled += 1;
        }

        fn events(&mut self) -> Vec<Event> {
            std::mem::take(&mut self.pending)
        }
    }

    #[test]
    fn dispatch_forwards_run_and_cancel_but_stops_on_quit() {
        let fixture = crate::test_support::fixture_env("dispatch");
        let app = fixture_app(fixture.root());
        let mut driver = FakeDriver::default();

        assert!(dispatch(&app, Action::Run("listProducts".to_string()), &mut driver).unwrap());
        assert!(dispatch(&app, Action::Cancel, &mut driver).unwrap());
        assert!(!dispatch(&app, Action::Quit, &mut driver).unwrap());

        assert_eq!(driver.started, vec![("listProducts".to_string(), None)]);
        assert_eq!(driver.cancelled, 1);
    }

    fn fixture_app(root: &std::path::Path) -> App {
        let options = testdet_core::config::LoadOptions {
            config: Some(root.join("testdet.yaml")),
            cwd: root.to_path_buf(),
            env: None,
        };
        let config = testdet_core::config::load_config(&options).expect("config loads");
        let resolved = testdet_core::config::resolve(&config, None, &[]).expect("env resolves");
        App::new(config, resolved)
    }
}
