use serde::Deserialize;
use std::collections::BTreeMap;
use crossterm::{cursor, execute, terminal::ClearType};
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const CAT_STATES: &str = include_str!("cat_states.json");
const SECRET_ROLL_RANGE: u64 = 2_000;
const KEBAB_ROLL_LIMIT: u64 = 3;
const SECRET_ROLL_LIMIT: u64 = 1;

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

#[derive(Debug, Deserialize)]
struct CatStates {
    width: usize,
    height: usize,
    default_state: String,
    states: BTreeMap<String, CatSprite>,
}

#[derive(Debug, Deserialize)]
struct CatSprite {
    lines: Vec<String>,
}

pub(crate) struct CatPresenter {
    writer: Option<Box<dyn Write>>,
    states: Option<CatStates>,
    last_state: Option<ValidationState>,
    rendered: bool,
}

impl CatPresenter {
    pub(crate) fn new() -> Self {
        if std::env::var_os("GITRUN_NO_CAT").is_some() {
            return Self {
                writer: None,
                states: None,
                last_state: None,
                rendered: false,
            };
        }

        let states = serde_json::from_str::<CatStates>(CAT_STATES)
            .ok()
            .filter(validate_states);

        let writer = states.as_ref().and_then(|_| terminal_writer());
        Self {
            writer,
            states,
            last_state: None,
            rendered: false,
        }
    }

    pub(crate) fn transition(&mut self, state: ValidationState) {
        if self.last_state == Some(state) {
            return;
        }

        let state_name = choose_cat_state(state);
        self.show_named(state_name);
        self.last_state = Some(state);
    }

    pub(crate) fn show_named(&mut self, state_name: &str) {
        let Some(writer) = self.writer.as_mut() else {
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
        let Some(sprite) = states.states.get(state_name) else {
            return;
        };

        if self.rendered {
            let _ = execute!(writer, cursor::SavePosition, cursor::MoveUp(4));
        }

        let _ = execute!(writer, cursor::MoveToColumn(0));
        for (index, line) in sprite.lines.iter().enumerate() {
            let _ = execute!(writer, crossterm::terminal::Clear(ClearType::CurrentLine));
            let _ = writer.write_all(line.as_bytes());
            let _ = writer.write_all(b"\r\n");
            if index + 1 == sprite.lines.len() {
                let _ = execute!(writer, crossterm::terminal::Clear(ClearType::CurrentLine));
            }
        }
        let _ = writer.write_all(b"\r\n");

        if self.rendered {
            let _ = execute!(writer, cursor::RestorePosition);
        }
        let _ = writer.flush();
        self.rendered = true;
    }

    pub(crate) fn finish(&mut self, exit_code: i32) {
        self.transition(if exit_code == 0 {
            ValidationState::Success
        } else {
            ValidationState::Failure
        });
    }
}

fn terminal_writer() -> Option<Box<dyn Write>> {
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
            .map(|file| Box::new(file) as Box<dyn Write>)
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
            ValidationState::Failure,
            ValidationState::Unknown,
        ];

        for state in states {
            assert!(!choose_cat_state(state).is_empty());
        }
    }

    #[test]
    fn cat_states_file_matches_declared_geometry() {
        let states = serde_json::from_str::<CatStates>(CAT_STATES).unwrap();
        assert!(validate_states(&states));
    }

    #[test]
    fn secret_roll_range_is_tiny() {
        assert!(SECRET_ROLL_LIMIT < KEBAB_ROLL_LIMIT);
        assert!(KEBAB_ROLL_LIMIT < 10);
        assert_eq!(SECRET_ROLL_RANGE, 2_000);
    }
}
