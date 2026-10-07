use crossterm::terminal;
use ratatui::{
    backend::CrosstermBackend,
    layout::Position,
    widgets::Paragraph,
    Terminal, TerminalOptions, Viewport,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CAT_STATES: &str = include_str!("cat_states.json");
const SECRET_ROLL_RANGE: u64 = 2_000;
const KEBAB_ROLL_LIMIT: u64 = 3;
const SECRET_ROLL_LIMIT: u64 = 1;
const LIVE_REFRESH: Duration = Duration::from_millis(900);
const LIVE_PANEL_HEIGHT: u16 = 4;
const LIVE_MIN_ROWS: u16 = LIVE_PANEL_HEIGHT + 4;

static ROLL_COUNTER: AtomicU64 = AtomicU64::new(0);

type LiveTerminal = Terminal<CrosstermBackend<io::Stdout>>;
type SharedTerminal = Arc<Mutex<LiveTerminal>>;

static ACTIVE_TERMINAL: OnceLock<Mutex<Option<SharedTerminal>>> = OnceLock::new();

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidationState {
    Ready,
    Reading,
    Working,
    Loading,
    Setup,
    Connecting,
    Updating,
    Recovering,
    Validating,
    Testing,
    Building,
    Compiling,
    Running,
    Waiting,
    Api,
    Unauthorized,
    Warning,
    Success,
    Failure,
    Unknown,
}

#[derive(Debug, Deserialize, Clone)]
struct CatStates {
    width: usize,
    height: usize,
    default_state: String,
    states: BTreeMap<String, CatSprite>,
}

#[derive(Debug, Deserialize, Clone)]
struct CatSprite {
    lines: Vec<String>,
}

#[derive(Debug, Clone)]
enum LiveSelection {
    Validation(ValidationState),
    Named(String),
}

struct LiveHandle {
    stop: Arc<AtomicBool>,
    wake: Arc<Condvar>,
    selection: Arc<Mutex<LiveSelection>>,
    terminal: SharedTerminal,
    join: Option<JoinHandle<()>>,
}

pub(crate) struct CatPresenter {
    states: Option<Arc<CatStates>>,
    last_state: Option<ValidationState>,
    live: Option<LiveHandle>,
}

impl Drop for CatPresenter {
    fn drop(&mut self) {
        self.stop_live();
    }
}

impl CatPresenter {
    pub(crate) fn new() -> Self {
        if std::env::var_os("GITRUN_NO_CAT").is_some() {
            return Self {
                states: None,
                last_state: None,
                live: None,
            };
        }

        let states = serde_json::from_str::<CatStates>(CAT_STATES)
            .ok()
            .filter(validate_states)
            .map(Arc::new);

        Self {
            states,
            last_state: None,
            live: None,
        }
    }

    pub(crate) fn start_live(&mut self) -> bool {
        if self.live.is_some() {
            return true;
        }

        let Some(states) = self.states.as_ref() else {
            return false;
        };
        let Ok((_, rows)) = terminal::size() else {
            return false;
        };
        if rows < LIVE_MIN_ROWS {
            return false;
        }

        let terminal = match Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(LIVE_PANEL_HEIGHT),
            },
        ) {
            Ok(terminal) => Arc::new(Mutex::new(terminal)),
            Err(_) => return false,
        };

        let selection = Arc::new(Mutex::new(
            self.last_state
                .map(LiveSelection::Validation)
                .unwrap_or_else(|| LiveSelection::Named(states.default_state.clone())),
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Condvar::new());
        let wake_guard = Arc::new(Mutex::new(()));

        activate_terminal(Arc::clone(&terminal));

        let thread_terminal = Arc::clone(&terminal);
        let thread_states = Arc::clone(states);
        let thread_selection = Arc::clone(&selection);
        let thread_stop = Arc::clone(&stop);
        let thread_wake = Arc::clone(&wake);
        let thread_guard = Arc::clone(&wake_guard);

        let join = thread::spawn(move || {
            let mut tick = 0usize;
            loop {
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }

                let current = thread_selection
                    .lock()
                    .map(|selection| selection.clone())
                    .unwrap_or_else(|_| LiveSelection::Named(thread_states.default_state.clone()));

                render_live_frame(&thread_terminal, &thread_states, &current, tick, false);
                tick = tick.wrapping_add(1);

                let Ok(guard) = thread_guard.lock() else {
                    break;
                };
                let _ = thread_wake.wait_timeout(guard, LIVE_REFRESH);
            }
        });

        self.live = Some(LiveHandle {
            stop,
            wake,
            selection,
            terminal,
            join: Some(join),
        });

        true
    }

    pub(crate) fn transition(&mut self, state: ValidationState) {
        if self.last_state == Some(state) {
            return;
        }

        let state_name = choose_cat_state(state);
        if self.live.is_some() {
            self.set_live_selection(LiveSelection::Validation(state));
        } else {
            self.show_named(state_name);
        }
        self.last_state = Some(state);
    }

    pub(crate) fn show_named(&mut self, state_name: &str) {
        let Some(states) = self.states.as_ref() else {
            return;
        };
        let state_name = if states.states.contains_key(state_name) {
            state_name
        } else {
            &states.default_state
        };

        if self.live.is_some() {
            self.set_live_selection(LiveSelection::Named(state_name.to_owned()));
            return;
        }

        let Some(sprite) = states.states.get(state_name) else {
            return;
        };

        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(b"
");
        for line in &sprite.lines {
            let _ = stdout.write_all(line.as_bytes());
            let _ = stdout.write_all(b"
");
        }
        let _ = stdout.write_all(b"
");
        let _ = stdout.flush();
    }

    pub(crate) fn finish(&mut self, exit_code: i32) {
        let final_state = if exit_code == 0 {
            ValidationState::Success
        } else {
            ValidationState::Failure
        };
        let final_name = choose_cat_state(final_state);

        if let Some(mut live) = self.live.take() {
            live.stop.store(true, Ordering::Relaxed);
            live.wake.notify_one();

            if let Some(join) = live.join.take() {
                let _ = join.join();
            }

            let selection = LiveSelection::Named(final_name.to_owned());
            render_live_frame(&live.terminal, self.states.as_deref().unwrap(), &selection, 0, true);
            deactivate_terminal(&live.terminal);
        } else {
            self.show_named(final_name);
        }

        self.last_state = Some(final_state);
    }

    fn set_live_selection(&self, selection: LiveSelection) {
        let Some(live) = self.live.as_ref() else {
            return;
        };

        if let Ok(mut current) = live.selection.lock() {
            *current = selection;
        }
        live.wake.notify_one();
    }

    fn stop_live(&mut self) {
        let Some(mut live) = self.live.take() else {
            return;
        };

        live.stop.store(true, Ordering::Relaxed);
        live.wake.notify_one();

        if let Some(join) = live.join.take() {
            let _ = join.join();
        }

        deactivate_terminal(&live.terminal);
    }
}

pub(crate) fn terminal_print(args: fmt::Arguments<'_>, stderr: bool, newline: bool) {
    let mut text = args.to_string();
    if newline {
        text.push('\n');
    }

    if let Some(terminal) = active_terminal() {
        let height = text.lines().count().max(1).min(u16::MAX as usize) as u16;
        let _ = terminal.lock().map(|mut terminal| {
            let text = text.as_str();
            terminal.insert_before(height, |buffer| {
                Paragraph::new(text).render(buffer.area, buffer);
            })
        });
        return;
    }

    if stderr {
        let mut stream = io::stderr().lock();
        let _ = stream.write_all(text.as_bytes());
        let _ = stream.flush();
    } else {
        let mut stream = io::stdout().lock();
        let _ = stream.write_all(text.as_bytes());
        let _ = stream.flush();
    }
}

fn active_terminal() -> Option<SharedTerminal> {
    ACTIVE_TERMINAL
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|active| active.clone()))
}

fn activate_terminal(terminal: SharedTerminal) {
    let slot = ACTIVE_TERMINAL.get_or_init(|| Mutex::new(None));
    if let Ok(mut active) = slot.lock() {
        *active = Some(terminal);
    }
}

fn deactivate_terminal(terminal: &SharedTerminal) {
    let Some(slot) = ACTIVE_TERMINAL.get() else {
        return;
    };
    let Ok(mut active) = slot.lock() else {
        return;
    };

    if active
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, terminal))
    {
        *active = None;
    }
}

fn validate_states(states: &CatStates) -> bool {
    if states.width == 0 || states.height == 0 {
        return false;
    }
    if !states.states.contains_key(&states.default_state) {
        return false;
    }
    states.states.values().all(|sprite| {
        sprite.lines.len() == states.height
            && sprite
                .lines
                .iter()
                .all(|line| line.chars().count() == states.width && line.is_ascii())
    })
}

fn choose_cat_state(state: ValidationState) -> &'static str {
    if state == ValidationState::Success {
        match roll(SECRET_ROLL_RANGE) {
            roll if roll < SECRET_ROLL_LIMIT => return "secret",
            roll if roll < KEBAB_ROLL_LIMIT => return "kebab",
            roll if roll < 500 => return "stars",
            roll if roll < 800 => return "happy",
            _ => return "success",
        }
    }

    match state {
        ValidationState::Ready => "kitty",
        ValidationState::Reading => "reading",
        ValidationState::Working => "working",
        ValidationState::Loading => "loading",
        ValidationState::Setup => "setup",
        ValidationState::Connecting => "github",
        ValidationState::Updating => "updating",
        ValidationState::Recovering => "recovery",
        ValidationState::Validating => "debugging",
        ValidationState::Testing => "testing",
        ValidationState::Building => "building",
        ValidationState::Compiling => "compiling",
        ValidationState::Running => "runner",
        ValidationState::Waiting => "waiting",
        ValidationState::Api => "github",
        ValidationState::Unauthorized => "scared",
        ValidationState::Warning => "confused",
        ValidationState::Failure => "failed",
        ValidationState::Unknown => "seated",
        ValidationState::Success => unreachable!("success handled above"),
    }
}

fn live_state_name(state: ValidationState, tick: usize) -> &'static str {
    let sequence: &'static [&'static str] = match state {
        ValidationState::Ready => &["kitty", "seated", "cute"],
        ValidationState::Reading => &["reading", "thinking"],
        ValidationState::Working => &["working", "typing", "coding"],
        ValidationState::Loading => &["loading", "waiting", "walking"],
        ValidationState::Setup => &["setup", "working", "building"],
        ValidationState::Connecting => &["github", "loading", "waiting"],
        ValidationState::Updating => &["updating", "working", "building"],
        ValidationState::Recovering => &["recovery", "fixing", "working"],
        ValidationState::Validating => &["debugging", "thinking", "working"],
        ValidationState::Testing => &["testing", "thinking", "working"],
        ValidationState::Building => &["building", "working", "typing"],
        ValidationState::Compiling => &["compiling", "rust", "coding"],
        ValidationState::Running => &["runner", "working", "typing"],
        ValidationState::Waiting => &["waiting", "sleeping", "thinking"],
        ValidationState::Api => &["github", "loading", "typing"],
        ValidationState::Unauthorized => &["scared", "confused"],
        ValidationState::Warning => &["confused", "thinking"],
        ValidationState::Success => &["success"],
        ValidationState::Failure => &["failed", "sad"],
        ValidationState::Unknown => &["seated", "unknown", "curious"],
    };
    sequence[tick % sequence.len()]
}

fn render_live_frame(
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    states: &CatStates,
    selection: &LiveSelection,
    tick: usize,
) {
    let Ok((_, rows)) = terminal::size() else {
        return;
    };
    if rows < LIVE_MIN_ROWS {
        return;
    }

    let state_name = match selection {
        LiveSelection::Validation(state) => live_state_name(*state, tick),
        LiveSelection::Named(name) => name.as_str(),
    };
    let state_name = if states.states.contains_key(state_name) {
        state_name
    } else {
        &states.default_state
    };
    let Some(sprite) = states.states.get(state_name) else {
        return;
    };

    // Reserve one blank row between normal command output and the live cat.
    // The scrolling region ends above that gap, so terminal output can never
    // overwrite the cat or use its row as part of the normal output stream.
    let scroll_bottom = rows.saturating_sub(LIVE_PANEL_HEIGHT);
    let panel_top = scroll_bottom.saturating_add(2);
    let mut output = format!("\x1b[s\x1b[1;{}r", scroll_bottom);
    for (offset, line) in (0u16..).zip(sprite.lines.iter()).take(3) {
        let row = panel_top.saturating_add(offset);
        output.push_str(&format!("\x1b[{};1H\x1b[2K{}", row, line));
    }
    output.push_str(&format!("\x1b[{};1H\x1b[2K\x1b[u", rows));

    if let Ok(mut writer) = writer.lock() {
        let _ = writer.write_all(output.as_bytes());
        let _ = writer.flush();
    }
}

fn clear_live_panel(writer: &Arc<Mutex<Box<dyn Write + Send>>>) {
    let Ok((_, rows)) = terminal::size() else {
        return;
    };

    let first = rows.saturating_sub(LIVE_PANEL_HEIGHT) + 1;
    let mut output = String::from("\x1b[s\x1b[r");
    for row in first..=rows {
        output.push_str(&format!("\x1b[{};1H\x1b[2K", row));
    }
    output.push_str("\x1b[u");

    if let Ok(mut writer) = writer.lock() {
        let _ = writer.write_all(output.as_bytes());
        let _ = writer.flush();
    }
}

fn render_live_frame(
    terminal: &SharedTerminal,
    states: &CatStates,
    selection: &LiveSelection,
    tick: usize,
    show_cursor: bool,
) {
    let state_name = match selection {
        LiveSelection::Validation(state) => live_state_name(*state, tick),
        LiveSelection::Named(name) => name.as_str(),
    };
    let state_name = if states.states.contains_key(state_name) {
        state_name
    } else {
        &states.default_state
    };
    let Some(sprite) = states.states.get(state_name) else {
        return;
    };

    let Ok(mut terminal) = terminal.lock() else {
        return;
    };

    let _ = terminal.draw(|frame| {
        let area = frame.area();
        let lines = sprite
            .lines
            .iter()
            .take(area.height as usize)
            .cloned()
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines.join("\n")), area);

        if show_cursor {
            frame.set_cursor_position(Position::new(
                0,
                area.height.saturating_sub(1),
            ));
        }
    });

    if show_cursor {
        let _ = terminal.show_cursor();
    } else {
        let _ = terminal.hide_cursor();
    }
}

fn roll(max: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default();
    let pid = std::process::id() as u64;
    let counter = ROLL_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut value = now ^ pid.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ counter;
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^= value >> 31;
    value % max
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_validation_state_has_a_cat_mapping() {
        let states = [
            ValidationState::Ready,
            ValidationState::Reading,
            ValidationState::Working,
            ValidationState::Loading,
            ValidationState::Setup,
            ValidationState::Connecting,
            ValidationState::Updating,
            ValidationState::Recovering,
            ValidationState::Validating,
            ValidationState::Testing,
            ValidationState::Building,
            ValidationState::Compiling,
            ValidationState::Running,
            ValidationState::Waiting,
            ValidationState::Api,
            ValidationState::Unauthorized,
            ValidationState::Warning,
            ValidationState::Success,
            ValidationState::Failure,
            ValidationState::Unknown,
        ];
        for state in states {
            assert!(!choose_cat_state(state).is_empty());
            assert!(!live_state_name(state, 0).is_empty());
            assert!(!live_state_name(state, 1).is_empty());
            assert!(!live_state_name(state, 2).is_empty());
        }
    }

    #[test]
    fn cat_states_file_matches_declared_geometry() {
        let states = serde_json::from_str::<CatStates>(CAT_STATES).unwrap();
        assert!(validate_states(&states));
    }

    #[test]
    fn live_state_sequences_reference_real_sprites() {
        let states = serde_json::from_str::<CatStates>(CAT_STATES).unwrap();
        let validation_states = [
            ValidationState::Ready,
            ValidationState::Reading,
            ValidationState::Working,
            ValidationState::Loading,
            ValidationState::Setup,
            ValidationState::Connecting,
            ValidationState::Updating,
            ValidationState::Recovering,
            ValidationState::Validating,
            ValidationState::Testing,
            ValidationState::Building,
            ValidationState::Compiling,
            ValidationState::Running,
            ValidationState::Waiting,
            ValidationState::Api,
            ValidationState::Unauthorized,
            ValidationState::Warning,
            ValidationState::Success,
            ValidationState::Failure,
            ValidationState::Unknown,
        ];
        for state in validation_states {
            for tick in 0..6 {
                assert!(states.states.contains_key(live_state_name(state, tick)));
            }
        }
    }

    #[test]
    fn secret_roll_range_is_tiny() {
        assert_eq!(
            (SECRET_ROLL_LIMIT, KEBAB_ROLL_LIMIT, SECRET_ROLL_RANGE),
            (1, 3, 2_000)
        );
    }
}
