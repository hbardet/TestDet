//! The concrete [`Driver`]: a dedicated thread with its own runtime.
//!
//! The UI's poll loop must never wait on the network, so the executor runs on
//! a worker thread that owns a current-thread tokio runtime and a single
//! [`SessionStore`] (cookies and captured variables persist across runs, as in
//! `testdet run`). Commands go in over a std channel, events come back over
//! the unbounded tokio channel the [`RunContext`] already emits into, and
//! cancellation crosses the thread boundary through a shared token — the
//! worker is inside `block_on` while a run is in flight, where only a `select!`
//! on that token can stop it.

use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use testdet_core::config::Resolved;
use testdet_core::diagnostic::{Diagnostic, Severity, codes};
use testdet_core::redact::Redactor;
use testdet_core::session::{SessionStore, execute_for_user};
use testdet_core::{Event, EventSink, RunContext};
use testdet_tui::Driver;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

/// What the UI can ask the worker to do.
#[derive(Debug)]
enum Command {
    /// Run `operation` as `user` (`None` is anonymous).
    Start {
        operation: String,
        user: Option<String>,
    },
}

/// A [`Driver`] backed by a worker thread.
pub struct ExecutorDriver {
    commands: Option<std::sync::mpsc::Sender<Command>>,
    events: UnboundedReceiver<Event>,
    pending: Vec<Event>,
    token: Arc<Mutex<CancellationToken>>,
    worker: Option<JoinHandle<()>>,
}

impl ExecutorDriver {
    /// Spawn the worker.
    ///
    /// The worker builds its own [`RunContext`], so a cancelled token can be
    /// replaced without rebuilding the driver.
    pub fn spawn(
        resolved: Arc<Resolved>,
        redactor: Arc<Redactor>,
        user: Option<String>,
    ) -> std::io::Result<Self> {
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let token = Arc::new(Mutex::new(CancellationToken::new()));
        let seed = Seed {
            resolved,
            redactor,
            events: event_tx,
            user,
            token: token.clone(),
        };
        let worker = thread::Builder::new()
            .name("testdet-tui-executor".to_string())
            .spawn(move || run_worker(seed, command_rx))?;
        Ok(Self {
            commands: Some(command_tx),
            events: event_rx,
            pending: Vec::new(),
            token,
            worker: Some(worker),
        })
    }
}

impl Driver for ExecutorDriver {
    fn start(&mut self, operation: &str, user: Option<&str>) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::Start {
                operation: operation.to_string(),
                user: user.map(str::to_string),
            });
        }
    }

    fn cancel(&mut self) {
        // Straight through the shared token: the worker is inside `block_on`
        // while a run is in flight and will not read the channel until it
        // returns.
        lock(&self.token).cancel();
    }

    fn events(&mut self) -> Vec<Event> {
        while let Ok(event) = self.events.try_recv() {
            self.pending.push(event);
        }
        std::mem::take(&mut self.pending)
    }
}

impl Drop for ExecutorDriver {
    fn drop(&mut self) {
        // Closing the channel ends the worker's `recv` loop; join so a
        // half-finished run cannot outlive the UI.
        drop(self.commands.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Everything the worker needs to build a [`RunContext`] again.
struct Seed {
    resolved: Arc<Resolved>,
    redactor: Arc<Redactor>,
    events: EventSink,
    user: Option<String>,
    token: Arc<Mutex<CancellationToken>>,
}

impl Seed {
    /// A context whose cancellation token is shared with the UI.
    fn context(&self, token: CancellationToken) -> RunContext {
        let mut ctx = RunContext::new(self.resolved.clone(), self.redactor.clone())
            .with_user(self.user.clone())
            .with_retain_body(true)
            .with_events(Some(self.events.clone()));
        ctx.cancel = token;
        ctx
    }

    /// Replace the shared token with a fresh one and return it.
    ///
    /// A cancelled token stays cancelled, and `RunContext` owns its token, so
    /// re-arming means a new token *and* a new store.
    fn rearm(&self) -> CancellationToken {
        let fresh = CancellationToken::new();
        *lock(&self.token) = fresh.clone();
        fresh
    }
}

fn run_worker(seed: Seed, commands: std::sync::mpsc::Receiver<Command>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = seed.events.send(Event::Failed {
                diagnostic: Diagnostic {
                    code: codes::E_TRANSPORT,
                    severity: Severity::Error,
                    file: None,
                    span: None,
                    path: None,
                    message: format!("could not start the async runtime: {error}"),
                    hint: Some("the TUI cannot execute operations without a runtime".to_string()),
                },
            });
            return;
        }
    };

    let mut store = SessionStore::new(seed.context(lock(&seed.token).clone()));

    while let Ok(command) = commands.recv() {
        let Command::Start { operation, user } = command;
        let Some(operation) = seed.resolved.operations.get(&operation) else {
            continue;
        };
        runtime.block_on(execute_for_user(&mut store, user.as_deref(), operation));
        if lock(&seed.token).is_cancelled() {
            let token = seed.rearm();
            store = SessionStore::new(seed.context(token));
        }
    }
}

fn lock(token: &Arc<Mutex<CancellationToken>>) -> MutexGuard<'_, CancellationToken> {
    token.lock().unwrap_or_else(|poison| poison.into_inner())
}
