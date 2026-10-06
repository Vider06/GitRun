use crossterm::terminal;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
    join: Option<JoinHandle<()>>,
}

pub(crate) struct CatPresenter {
    writer: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
    states: Option<Arc<CatStates>>,
    last_state: Option<ValidationState>,
    live: Option<LiveHandle>,
}

impl CatPresenter {
    pub(crate) fn new() -> Self {
        if std::env::var_os("GITRUN_NO_CAT").is_some() {
            return Self {
                writer: None,
                states: None,
                last_state: None,
                live: None,
            };
        }

        let states = serde_json::from_str::<CatStates>(CAT_STATES)
            .ok()
            .filter(validate_states)
            .map(Arc::new);

        let writer = states
            .as_ref()
            .and_then(|_| terminal_writer())
            .map(|writer| Arc::new(Mutex::new(writer)));

        Self {
            writer,
            states,
            last_state: None,
            live: None,
        }
    }

    pub(crate) fn start_live(&mut self) -> bool {
        if self.live.is_some() {
            return true;
        }

        let (Some(writer), Some(states)) = (self.writer.as_ref(), self.states.as_ref()) else {
            return false;
        };

        let Ok((_, rows)) = terminal::size() else {
            return false;
        };
        if rows < LIVE_MIN_ROWS {
            return false;
        }

        let selection = Arc::new(Mutex::new(
            self.last_state
                .map(LiveSelection::Validation)
                .unwrap_or_else(|| LiveSelection::Named(states.default_state.clone())),
        ));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_writer = Arc::clone(writer);
        let thread_states = Arc::clone(states);
        let thread_selection = Arc::clone(&selection);
        let thread_stop = Arc::clone(&stop);

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

                render_live_frame(&thread_writer, &thread_states, &current, tick);
                tick = tick.wrapping_add(1);
                thread::sleep(LIVE_REFRESH);
            }
        });

        self.live = Some(LiveHandle {
            stop,
            join: Some(join),
        });

        let selection = self
            .last_state
            .map(LiveSelection::Validation)
            .unwrap_or_else(|| LiveSelection::Named(states.default_state.clone()));
        if let Ok(mut current) = selection.lock() {
            *current = self
                .last_state
                .map(LiveSelection::Validation)
                .unwrap_or_else(|| LiveSelection::Named(states.default_state.clone()));
        }
        render_live_frame(writer, states, &selection, 0);
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
        let Some(writer) = self.writer.as_ref() else {
            return;
        };
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

        let Ok(mut writer) = writer.lock() else {
            return;
        };

        let _ = writer.write_all(b"\r\n");
        for line in &sprite.lines {
            let _ = writer.write_all(line.as_bytes());
            let _ = writer.write_all(b"\r\n");
        }
        let _ = writer.write_all(b"\r\n");
        let _ = writer.flush();
    }

    pub(crate) fn finish(&mut self, exit_code: i32) {
        self.stop_live();
        self.transition(if exit_code == 0 {
            ValidationState::Success
        } else {
            ValidationState::Failure
        });
    }

    fn set_live_selection(&self, selection: LiveSelection) {
        let Some(live) = self.live.as_ref() else {
            return;
        };
        let current = LIVE_SELECTION
            .get_or_init(|| Arc::new(Mutex::new(LiveSelection::Named(String::new()))));
        let _ = current;

        if let Some(writer) = self.writer.as_ref() {
            if let Some(states) = self.states.as_ref() {
                if let Some(shared) = live_selection_registry(live) {
                    if let Ok(mut current) = shared.lock() {
                        *current = selection.clone();
                    }
                    render_live_frame(writer, states, &selection, 0);
                    return;
                }
            }
        }
    }

    fn stop_live(&mut self) {
        let Some(mut live) = self.live.take() else {
            return;
        };

        live.stop.store(true, Ordering::Relaxed);
        if let Some(join) = live.join.take() {
            let _ = join.join();
        }

        if let Some(writer) = self.writer.as_ref() {
            clear_live_panel(writer);
        }
    }
}

fn terminal_writer() -> Option<Box<dyn Write + Send>> {
    if !io::stderr().is_terminal() && !io::stdout().is_terminal() {
        return None;
    }

    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        OpenOptions::new()
            .write(true)
            .open("/dev/tty")
            .ok()
            .map(|file| Box::new(file) as Box<dyn Write + Send>)
    }

    #[cfg(windows)]
    {
        Some(Box::new(io::stderr()))
    }

    #[cfg(not(any(unix, windows)))]
    {
        None
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
            && sprite.lines.iter().all(|line| {
                line.chars().count() == states.width && line.is_ascii()
            })
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

fn live_selection_registry(_live: &LiveHandle) -> Option<Arc<Mutex<LiveSelection>>> {
    None
}

static LIVE_SELECTION: std::sync::OnceLock<Arc<Mutex<LiveSelection>>> = std::sync::OnceLock::new();

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

    let panel_top = rows.saturating_sub(LIVE_PANEL_HEIGHT) + 1;
    let mut output = String::new();
    output.push_str("\x1b[s");
    output.push_str(&format!("\x1b[1;{}r", rows.saturating_sub(LIVE_PANEL_HEIGHT)));
    for (index, line) in sprite.lines.iter().enumerate().take(3) {
        let row = panel_top.saturating_add(index);
        output.push_str(&format!("\x1b[{};1H\x1b[2K{}", row, line));
    }
    output.push_str(&format!(
        "\x1b[{};1H\x1b[2K",
        rows.saturating_add(1)
    ));
    output.push_str("\x1b[u");

    if let Ok(mut writer) = writer.lock() {
        let _ = writer.write_all(output.as_bytes());
        let _ = writer.flush();
    }
}

fn clear_live_panel(writer: &Arc<Mutex<Box<dyn Write + Send>>>) {
    let Ok((_, rows)) = terminal::size() else {
        return;
    };

    let mut output = String::new();
    output.push_str("\x1b[s\x1b[r");
    let first = rows.saturating_sub(LIVE_PANEL_HEIGHT) + 1;
    for row in first..=rows {
        output.push_str(&format!("\x1b[{};1H\x1b[2K", row));
    }
    output.push_str("\x1b[u");

    if let Ok(mut writer) = writer.lock() {
        let _ = writer.write_all(output.as_bytes());
        let _ = writer.flush();
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
        assert!(SECRET_ROLL_LIMIT < KEBAB_ROLL_LIMIT);
        assert!(KEBAB_ROLL_LIMIT < 10);
        assert_eq!(SECRET_ROLL_RANGE, 2_000);
    }
}
