//! Application state: the operation tree, the selection and the key actions.
//!
//! The `App` owns everything the drawn frame needs and nothing that talks to
//! the network: a key press returns an [`Action`] and the caller (the event
//! loop, or a test) decides what to do with it. Events produced by a run are
//! fed back in through [`App::apply_event`], which is the seam Task 21's
//! response viewer builds on.

use std::collections::BTreeMap;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use testdet_core::Event;
use testdet_core::config::{Config, Resolved};
use testdet_core::operation::model::Operation;
use testdet_core::outcome::Outcome;
use testdet_core::redact::Redactor;
use testdet_core::serde_json::Value;

use crate::body_view::BodyView;
use crate::keys;
use crate::request_pane;
use crate::response_pane::{self, Tab};
use crate::tree;
use crate::var_store::VarStore;

/// What a key press asks the frontend to do.
///
/// `handle_key` applies the state change it can apply on its own (selection,
/// filter text, user and environment cursor) and returns the action the
/// caller may forward to a [`Driver`](crate::Driver).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do.
    None,
    /// Run the named operation.
    Run(String),
    /// Leave the UI.
    Quit,
    /// Cancel the in-flight run.
    Cancel,
    /// Switch to the named user; an empty name means anonymous.
    SwitchUser(String),
    /// Switch to the named environment.
    SwitchEnv(String),
    /// Store a selected response node as a variable (Task 21 owns the source).
    StoreVar,
    /// The filter query changed (live filtering happens on each keystroke).
    Filter(String),
}

/// One directory of operations in the tree.
#[derive(Debug)]
pub(crate) struct Group {
    /// The operation file's parent directory, relative to the config root.
    pub(crate) name: String,
    /// Operation ids in this group, sorted by id.
    pub(crate) operations: Vec<String>,
}

/// The TUI's whole state.
pub struct App {
    pub(crate) config: Config,
    pub(crate) resolved: Resolved,
    pub(crate) groups: Vec<Group>,
    /// The operations that survive the current filter, in display order.
    pub(crate) visible: Vec<String>,
    /// Index into [`App::visible`].
    pub(crate) selected: usize,
    pub(crate) filter: String,
    pub(crate) filtering: bool,
    pub(crate) user: Option<String>,
    pub(crate) env: String,
    /// The operation currently in flight, once one was started.
    pub(crate) running: Option<String>,
    /// Every event received so far, for Task 21's response viewer.
    pub(crate) events: Vec<Event>,
    /// The last status line, shown in the banner.
    pub(crate) status: Option<String>,
    /// The last finished outcome, redacted, as the response pane renders it.
    pub(crate) outcome: Option<Outcome>,
    /// That outcome's body, already turned into display lines.
    pub(crate) body: Option<BodyView>,
    /// Which response tab is showing.
    pub(crate) tab: Tab,
    /// Variables the user stored with `s`; in memory, this session only.
    pub(crate) vars: VarStore,
    /// The redactor every rendered header, cookie and body passes through.
    pub(crate) redactor: Arc<Redactor>,
}

impl App {
    /// Build the app from a loaded config and its resolved environment.
    #[must_use]
    pub fn new(config: Config, resolved: Resolved) -> Self {
        let groups = group_operations(&config);
        let visible = groups
            .iter()
            .flat_map(|group| group.operations.iter().cloned())
            .collect();
        let env = resolved.env_name.clone();
        Self {
            config,
            resolved,
            groups,
            visible,
            selected: 0,
            filter: String::new(),
            filtering: false,
            user: None,
            env,
            running: None,
            events: Vec::new(),
            status: None,
            outcome: None,
            body: None,
            tab: Tab::Body,
            vars: VarStore::new(),
            redactor: Arc::new(Redactor::new()),
        }
    }

    /// Select the user to start as. `None` is anonymous.
    ///
    /// `App::new` is pinned to `(config, resolved)`, which cannot carry the
    /// `--user` argument, so the CLI applies it through this builder.
    #[must_use]
    pub fn with_user(mut self, user: Option<String>) -> Self {
        self.user = user;
        self
    }

    /// Share the caller's redactor instead of an empty one.
    ///
    /// The TUI itself never builds a redactor (that would read secrets), so the
    /// host — which already has one for the runs it dispatches — passes it in.
    #[must_use]
    pub fn with_redactor(mut self, redactor: Arc<Redactor>) -> Self {
        self.redactor = redactor;
        self
    }

    /// Interpret one key press.
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Action {
        keys::handle_key(self, key)
    }

    /// Render the frame: tree on the left, request and response on the right.
    pub fn draw(&self, frame: &mut Frame<'_>) {
        let [left, right] =
            Layout::horizontal([Constraint::Length(32), Constraint::Min(0)]).areas(frame.area());
        tree::draw(self, frame, left);

        let [request, response] =
            Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(right);
        request_pane::draw(self, frame, request);
        response_pane::draw(self, frame, response);
    }

    /// Feed one engine event into the app.
    ///
    /// Task 21 reads [`App::events`] to render the response; for now the
    /// event only drives the in-flight flag and the banner's status line.
    pub fn apply_event(&mut self, event: Event) {
        match &event {
            Event::Started { operation, .. } => {
                self.running = Some(operation.clone());
                self.status = Some(format!("running {operation}"));
            }
            Event::Sent { bytes } => self.status = Some(format!("sent {bytes} bytes")),
            Event::Received {
                status, elapsed, ..
            } => {
                self.status = Some(format!("{status} in {} ms", elapsed.as_millis()));
            }
            Event::Failed { diagnostic } => {
                self.running = None;
                self.status = Some(diagnostic.message.clone());
            }
            Event::Finished { outcome } => {
                self.running = None;
                self.status = Some(format!("{}: HTTP {}", outcome.operation, outcome.status));
                let mut body = body_of(outcome);
                body.set_filter(Some(&self.filter));
                self.body = Some(body);
                self.outcome = Some(outcome.clone());
                self.tab = Tab::Body;
            }
        }
        self.events.push(event);
    }

    /// Apply the config the host re-resolved for the new environment (C2).
    ///
    /// The response pane is cleared: an outcome, its cookies and its
    /// variables all belong to the environment they came from, so showing them
    /// under the new banner would be a lie.
    pub fn apply_resolved(&mut self, resolved: Resolved) {
        self.env = resolved.env_name.clone();
        self.resolved = resolved;
        self.outcome = None;
        self.body = None;
        self.status = Some(format!("environment: {}", self.env));
    }

    /// Put a host message (e.g. a failed environment switch) in the banner.
    pub fn report(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
    }

    /// The operation id the cursor is on.
    #[must_use]
    pub fn selected_operation(&self) -> Option<&str> {
        self.visible.get(self.selected).map(String::as_str)
    }

    /// The user operations run as; `None` is anonymous.
    #[must_use]
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// Whether a run is in flight (so `Esc` means cancel).
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The current filter query.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Whether the filter prompt has the keyboard.
    #[must_use]
    pub fn filtering(&self) -> bool {
        self.filtering
    }

    /// The environment the config was resolved for.
    #[must_use]
    pub fn environment(&self) -> &str {
        &self.env
    }

    /// The last status line, if any event or key produced one.
    #[must_use]
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Every event received so far.
    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The last finished outcome, if any.
    #[must_use]
    pub fn outcome(&self) -> Option<&Outcome> {
        self.outcome.as_ref()
    }

    /// The last finished body, already prepared for display.
    #[must_use]
    pub fn body(&self) -> Option<&BodyView> {
        self.body.as_ref()
    }

    /// The operation a run is waiting on, once one was started.
    #[must_use]
    pub fn running_operation(&self) -> Option<&str> {
        self.running.as_deref()
    }

    /// Variables stored with `s` in this session.
    #[must_use]
    pub fn vars(&self) -> &VarStore {
        &self.vars
    }

    /// The variable map a run of `operation` would use: the resolved stack
    /// (operation → env → user → `--var`) with the stored values layered on
    /// top, so a stored value always wins (§7.19).
    #[must_use]
    pub fn vars_for(&self, operation: &str) -> BTreeMap<String, Value> {
        let mut vars = self
            .config
            .operations
            .get(operation)
            .map_or_else(BTreeMap::new, |operation| {
                self.resolved.vars_for(operation, self.user.as_deref())
            });
        vars.extend(self.vars.as_overrides());
        vars
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn tab(&self) -> Tab {
        self.tab
    }

    /// Scrub one rendered value. Used for every header and cookie value the
    /// pane prints: the engine already redacted the outcome, and redacting
    /// again is idempotent, so a value can never slip through raw.
    pub(crate) fn scrub(&self, value: &str) -> String {
        self.redactor.scrub(value)
    }

    /// Flip the body between expanded and collapsed.
    pub(crate) fn toggle_collapse(&mut self) {
        if let Some(body) = &mut self.body {
            body.toggle_collapsed();
        }
    }

    /// Show the next response tab.
    pub(crate) fn next_tab(&mut self) {
        self.tab = self.tab.next();
    }

    /// Show the previous response tab.
    pub(crate) fn previous_tab(&mut self) {
        self.tab = self.tab.previous();
    }

    /// Store the last response's fields as variables (`s`).
    ///
    /// A GraphQL `data` object is unwrapped first — `{"data":{"token":"…"}}`
    /// stores `token`, not `data`. Anything that is not a JSON object is stored
    /// whole under `response`. Values are stored already redacted: they came
    /// out of the outcome.
    pub(crate) fn store_selection(&mut self) {
        let Some(outcome) = &self.outcome else {
            self.status = Some("nothing to store: no response yet".to_string());
            return;
        };
        let Some(body) = &outcome.body else {
            self.status = Some("nothing to store: the body was not retained".to_string());
            return;
        };
        let Ok(value) = testdet_core::serde_json::from_slice::<Value>(body) else {
            self.status = Some("nothing to store: the response is not JSON".to_string());
            return;
        };
        let operation = outcome.operation.clone();
        let source = format!("{operation} response");
        let payload = match &value {
            Value::Object(map) => map.get("data").filter(|data| data.is_object()),
            _ => None,
        };
        let mut stored = 0;
        match payload.unwrap_or(&value) {
            Value::Object(map) => {
                for (name, value) in map {
                    self.vars.store(name, value.clone(), source.clone());
                    stored += 1;
                }
            }
            other => {
                self.vars.store("response", other.clone(), source);
                stored = 1;
            }
        }
        self.status = Some(if stored == 0 {
            "nothing stored: the response has no fields".to_string()
        } else {
            format!("stored {stored} variable(s) from {operation}")
        });
    }

    pub(crate) fn resolved(&self) -> &Resolved {
        &self.resolved
    }

    pub(crate) fn selection(&self) -> Option<&str> {
        self.selected_operation()
    }

    /// Move the cursor down, stopping at the last visible operation.
    pub(crate) fn select_next(&mut self) {
        if !self.visible.is_empty() && self.selected + 1 < self.visible.len() {
            self.selected += 1;
        }
    }

    /// Move the cursor up, stopping at the first visible operation.
    pub(crate) fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Enter filter mode with an empty query.
    pub(crate) fn enter_filter(&mut self) {
        self.filtering = true;
        self.filter.clear();
        self.refilter();
    }

    /// Leave filter mode, keeping the query (the tree stays narrowed).
    pub(crate) fn confirm_filter(&mut self) {
        self.filtering = false;
    }

    /// Leave filter mode and drop the query.
    pub(crate) fn clear_filter(&mut self) {
        self.filtering = false;
        self.filter.clear();
        self.refilter();
    }

    /// Append one character to the query and narrow the tree live.
    pub(crate) fn push_filter(&mut self, character: char) {
        self.filter.push(character);
        self.refilter();
    }

    /// Drop the query's last character and widen the tree live.
    pub(crate) fn pop_filter(&mut self) {
        self.filter.pop();
        self.refilter();
    }

    /// Ask for a run of the selection; sets the in-flight flag optimistically.
    pub(crate) fn begin_run(&mut self) -> Action {
        if self.running.is_some() {
            return Action::None;
        }
        match self.selected_operation().map(str::to_string) {
            Some(id) => {
                self.running = Some(id.clone());
                Action::Run(id)
            }
            None => Action::None,
        }
    }

    /// Cycle to the next user: anonymous, then each declared user in order.
    pub(crate) fn cycle_user(&mut self) -> String {
        let mut names: Vec<Option<String>> = vec![None];
        names.extend(self.resolved.users.keys().cloned().map(Some));
        let current = names
            .iter()
            .position(|name| *name == self.user)
            .unwrap_or(0);
        let next = names[(current + 1) % names.len()].clone();
        self.user = next.clone();
        next.unwrap_or_default()
    }

    /// The next environment the manifest declares.
    ///
    /// The name is only *proposed*: switching needs the config re-resolved
    /// (and the environment's secrets reloaded), which the host does. The
    /// banner changes when [`App::apply_resolved`] succeeds, so an unknown
    /// environment never leaves the UI claiming to be somewhere it is not.
    pub(crate) fn next_env(&self) -> String {
        let names = self.environments();
        let current = names.iter().position(|name| *name == self.env).unwrap_or(0);
        names[(current + 1) % names.len()].clone()
    }

    /// The selectable environments: the declared ones, plus a synthetic
    /// `local` when the manifest declares none, plus the current name.
    fn environments(&self) -> Vec<String> {
        let mut names: Vec<String> = self.config.manifest.environments.keys().cloned().collect();
        if names.is_empty() {
            names.push("local".to_string());
        }
        if !names.contains(&self.env) {
            names.push(self.env.clone());
        }
        names
    }

    /// Recompute the visible list, keeping the selection on the same
    /// operation when it survives, and falling back to the first match.
    fn refilter(&mut self) {
        let previous = self.selected_operation().map(str::to_string);
        let needle = self.filter.to_lowercase();
        let mut visible = Vec::new();
        for group in &self.groups {
            for id in &group.operations {
                let keep = needle.is_empty()
                    || self
                        .config
                        .operations
                        .get(id)
                        .is_some_and(|operation| operation_matches(operation, &needle));
                if keep {
                    visible.push(id.clone());
                }
            }
        }
        self.visible = visible;
        self.selected = previous
            .and_then(|id| self.visible.iter().position(|visible| *visible == id))
            .unwrap_or(0);
        if let Some(body) = &mut self.body {
            body.set_filter(Some(&self.filter));
        }
    }
}

/// Turn a finished outcome into a [`BodyView`], keeping the distinction between
/// "empty", "not retained" and "not UTF-8" (§7.16).
fn body_of(outcome: &Outcome) -> BodyView {
    let content_type = outcome
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.as_str());
    match (&outcome.body, outcome.body_retained) {
        (Some(bytes), _) => BodyView::from_bytes(bytes, content_type),
        (None, false) => BodyView::not_retained(outcome.body_size),
        (None, true) => BodyView::from_bytes(&[], content_type),
    }
}

/// Whether the operation matches the filter: id, description or any tag,
/// case-insensitive.
fn operation_matches(operation: &Operation, needle: &str) -> bool {
    operation.id.to_lowercase().contains(needle)
        || operation
            .description
            .as_deref()
            .is_some_and(|description| description.to_lowercase().contains(needle))
        || operation
            .tags
            .iter()
            .any(|tag| tag.to_lowercase().contains(needle))
}

/// Group the operations by their file's parent directory, relative to the
/// config root, and sort each group by id.
fn group_operations(config: &Config) -> Vec<Group> {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for id in config.operations.keys() {
        let name = config
            .operation_files
            .get(id)
            .and_then(|path| path.parent())
            .and_then(|parent| parent.strip_prefix(&config.root_dir).ok())
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        grouped.entry(name).or_default().push(id.clone());
    }
    grouped
        .into_iter()
        .map(|(name, mut operations)| {
            operations.sort();
            Group { name, operations }
        })
        .collect()
}
