//! The keymap: one function that turns a key press into an [`Action`].

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{Action, App};

/// Interpret `key` against `app`, applying the state change it can make and
/// returning the action the caller may forward.
///
/// While the filter prompt is open every printable character is part of the
/// query — `q` types a `q` instead of quitting — and `Esc` closes the prompt.
/// Everywhere else: `Enter` runs the selection, `Esc` cancels an in-flight run
/// (and does nothing when idle), `u` cycles users, `e` switches environments,
/// `s` stores the response's fields as variables, `c` collapses or expands the
/// body, `Tab`/`Shift-Tab` cycle the response tabs, `/` opens the filter, `q`
/// quits, and the arrow keys or `j`/`k` move the cursor.
pub(crate) fn handle_key(app: &mut App, key: KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return Action::None;
    }

    if app.filtering() {
        return match key.code {
            KeyCode::Esc => {
                app.clear_filter();
                Action::Filter(String::new())
            }
            KeyCode::Enter => {
                app.confirm_filter();
                Action::Filter(app.filter().to_string())
            }
            KeyCode::Backspace => {
                app.pop_filter();
                Action::Filter(app.filter().to_string())
            }
            KeyCode::Char(character) => {
                app.push_filter(character);
                Action::Filter(app.filter().to_string())
            }
            _ => Action::None,
        };
    }

    match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Esc if app.is_running() => Action::Cancel,
        KeyCode::Esc => Action::None,
        KeyCode::Enter => app.begin_run(),
        KeyCode::Char('u') => Action::SwitchUser(app.cycle_user()),
        KeyCode::Char('e') => Action::SwitchEnv(app.next_env()),
        KeyCode::Char('s') => {
            app.store_selection();
            Action::StoreVar
        }
        KeyCode::Char('c') => {
            app.toggle_collapse();
            Action::None
        }
        KeyCode::Tab => {
            app.next_tab();
            Action::None
        }
        KeyCode::BackTab => {
            app.previous_tab();
            Action::None
        }
        KeyCode::Char('/') => {
            app.enter_filter();
            Action::Filter(String::new())
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.select_next();
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.select_previous();
            Action::None
        }
        _ => Action::None,
    }
}
