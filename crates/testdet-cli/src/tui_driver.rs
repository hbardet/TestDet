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
//!
//! Two kinds of state change live here, and they are deliberately different:
//!
//! * **cancelling** (§7.10) stops one request and keeps the session: the store
//!   is reused and only its cancellation token is renewed, so `Esc` does not
//!   log the user out;
//! * **switching environment** (§7.16) discards the session: cookies belong to
//!   an environment, so a new store is the correct outcome there.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use testdet_core::config::{LoadOptions, Resolved, load_config, resolve};
use testdet_core::diagnostic::render_text;
use testdet_core::diagnostic::{Diagnostic, Severity, codes};
use testdet_core::redact::{Redactor, redactor_for};
use testdet_core::serde_json::Value;
use testdet_core::session::{SessionStore, execute_for_user};
use testdet_core::{Event, EventSink, RunContext};
use testdet_tui::Driver;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

/// What the UI can ask the worker to do.
///
/// No `Debug`: the switch carries a `Redactor`, which deliberately does not
/// print its patterns.
enum Command {
    /// Run `operation` as `user` (`None` is anonymous), with the values stored
    /// in the UI this session as the top variable layer (§7.16).
    Start {
        operation: String,
        user: Option<String>,
        vars: BTreeMap<String, Value>,
    },
    /// Adopt a re-resolved environment and start a fresh session store.
    Switch {
        resolved: Arc<Resolved>,
        redactor: Arc<Redactor>,
        token: CancellationToken,
    },
}

/// A [`Driver`] backed by a worker thread.
pub struct ExecutorDriver {
    commands: Option<std::sync::mpsc::Sender<Command>>,
    events: UnboundedReceiver<Event>,
    pending: Vec<Event>,
    token: Arc<Mutex<CancellationToken>>,
    worker: Option<JoinHandle<()>>,
    /// How to load the config again when the environment changes.
    loader: LoadOptions,
}

impl ExecutorDriver {
    /// Spawn the worker.
    ///
    /// `loader` is kept so [`Driver::switch_environment`] can re-load the
    /// config for another environment (including that environment's secrets
    /// file); the worker builds its own [`RunContext`], so a cancelled token
    /// can be replaced without rebuilding the driver.
    pub fn spawn(
        loader: LoadOptions,
        resolved: Arc<Resolved>,
        redactor: Arc<Redactor>,
        user: Option<String>,
    ) -> std::io::Result<Self> {
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let token = Arc::new(Mutex::new(CancellationToken::new()));
        let seed = Seed {
            events: event_tx,
            user,
            token: token.clone(),
        };
        let worker = thread::Builder::new()
            .name("testdet-tui-executor".to_string())
            .spawn(move || run_worker(seed, resolved, redactor, command_rx))?;
        Ok(Self {
            commands: Some(command_tx),
            events: event_rx,
            pending: Vec::new(),
            token,
            worker: Some(worker),
            loader,
        })
    }
}

impl Driver for ExecutorDriver {
    fn start(&mut self, operation: &str, user: Option<&str>, vars: &BTreeMap<String, Value>) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::Start {
                operation: operation.to_string(),
                user: user.map(str::to_string),
                vars: vars.clone(),
            });
        }
    }

    fn cancel(&mut self) {
        // Straight through the shared token: the worker is inside `block_on`
        // while a run is in flight and will not read the channel until it
        // returns. The token is renewed by the worker afterwards, so the next
        // request is not born cancelled.
        lock(&self.token).cancel();
    }

    fn switch_environment(&mut self, environment: &str) -> Result<Resolved, String> {
        let options = LoadOptions {
            env: Some(environment.to_string()),
            ..self.loader.clone()
        };
        let config = load_config(&options).map_err(|diagnostics| render_text(&diagnostics))?;
        let resolved = resolve(&config, Some(environment), &[])
            .map_err(|diagnostics| render_text(&diagnostics))?;
        let redactor = Arc::new(redactor_for(&config, &resolved));
        let token = CancellationToken::new();
        *lock(&self.token) = token.clone();
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::Switch {
                resolved: Arc::new(resolved.clone()),
                redactor,
                token,
            });
        }
        Ok(resolved)
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

/// The parts of a [`RunContext`] that survive a run, a cancellation and an
/// environment switch.
struct Seed {
    events: EventSink,
    user: Option<String>,
    token: Arc<Mutex<CancellationToken>>,
}

impl Seed {
    /// A context whose cancellation token is shared with the UI.
    fn context(
        &self,
        resolved: &Arc<Resolved>,
        redactor: &Arc<Redactor>,
        token: CancellationToken,
    ) -> RunContext {
        let mut ctx = RunContext::new(resolved.clone(), redactor.clone())
            .with_user(self.user.clone())
            .with_retain_body(true)
            .with_events(Some(self.events.clone()));
        ctx.cancel = token;
        ctx
    }
}

fn run_worker(
    seed: Seed,
    mut resolved: Arc<Resolved>,
    mut redactor: Arc<Redactor>,
    commands: std::sync::mpsc::Receiver<Command>,
) {
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

    let mut store =
        SessionStore::new(seed.context(&resolved, &redactor, lock(&seed.token).clone()));

    while let Ok(command) = commands.recv() {
        match command {
            Command::Start {
                operation,
                user,
                vars,
            } => {
                let Some(operation) = resolved.operations.get(&operation) else {
                    continue;
                };
                // The stored values are per session and in memory only: they
                // land in the store's context, never on disk (§7.16).
                store.set_var_overrides(vars);
                runtime.block_on(execute_for_user(&mut store, user.as_deref(), operation));
                if lock(&seed.token).is_cancelled() {
                    // C1: re-arm the token in place. Rebuilding the store here
                    // would drop every cookie and capture the session owns.
                    let token = store.renew_cancellation();
                    *lock(&seed.token) = token;
                }
            }
            Command::Switch {
                resolved: next,
                redactor: next_redactor,
                token,
            } => {
                resolved = next;
                redactor = next_redactor;
                store = SessionStore::new(seed.context(&resolved, &redactor, token));
            }
        }
    }
}

fn lock(token: &Arc<Mutex<CancellationToken>>) -> MutexGuard<'_, CancellationToken> {
    token.lock().unwrap_or_else(|poison| poison.into_inner())
}
