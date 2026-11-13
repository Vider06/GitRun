#[cfg(not(any(kani, verus)))]
use crossterm::{cursor::MoveToNextLine, execute, terminal};
#[cfg(not(any(kani, verus)))]
use ratatui::{
    backend::CrosstermBackend,
    layout::Position,
    widgets::{Clear, Paragraph, Widget, Wrap},
    Terminal, TerminalOptions, Viewport,
};
#[cfg(not(any(kani, verus)))]
use serde::Deserialize;
#[cfg(not(any(kani, verus)))]
use std::collections::BTreeMap;
#[cfg(not(any(kani, verus)))]
use std::fmt;
#[cfg(not(any(kani, verus)))]
use std::io::{self, IsTerminal, Write};
#[cfg(not(any(kani, verus)))]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(not(any(kani, verus)))]
use std::sync::{Arc, Condvar, Mutex, OnceLock};
#[cfg(not(any(kani, verus)))]
use std::thread::{self, JoinHandle};
#[cfg(not(any(kani, verus)))]
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(not(any(kani, verus)))]
const CAT_STATES: &str = include_str!("cat/cat_states.json");
#[cfg(not(any(kani, verus)))]
const SECRET_ROLL_RANGE: u64 = 2_000;
#[cfg(not(any(kani, verus)))]
const KEBAB_ROLL_LIMIT: u64 = 3;
#[cfg(not(any(kani, verus)))]
const SECRET_ROLL_LIMIT: u64 = 1;
#[cfg(not(any(kani, verus)))]
const LIVE_REFRESH: Duration = Duration::from_millis(900);
#[cfg(not(any(kani, verus)))]
const LIVE_PANEL_HEIGHT: u16 = 4;
#[cfg(not(any(kani, verus)))]
const LIVE_MIN_ROWS: u16 = LIVE_PANEL_HEIGHT + 4;

#[cfg(not(any(kani, verus)))]
static ROLL_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(not(any(kani, verus)))]
type LiveTerminal = Terminal<CrosstermBackend<io::Stdout>>;
#[cfg(not(any(kani, verus)))]
type SharedTerminal = Arc<Mutex<LiveTerminal>>;

#[cfg(not(any(kani, verus)))]
static ACTIVE_TERMINAL: OnceLock<Mutex<Option<SharedTerminal>>> = OnceLock::new();

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(not(any(kani, verus)))]
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
#[cfg(not(any(kani, verus)))]
struct CatStates {
    width: usize,
    height: usize,
    default_state: String,
    states: BTreeMap<String, CatSprite>,
}

#[derive(Debug, Deserialize, Clone)]
#[cfg(not(any(kani, verus)))]
struct CatSprite {
    lines: Vec<String>,
}

#[derive(Debug, Clone)]
#[cfg(not(any(kani, verus)))]
enum LiveSelection {
    Validation(ValidationState),
    Named(String),
}

#[cfg(not(any(kani, verus)))]
struct LiveHandle {
    stop: Arc<AtomicBool>,
    wake: Arc<Condvar>,
    selection: Arc<Mutex<LiveSelection>>,
    terminal: SharedTerminal,
    join: Option<JoinHandle<()>>,
    signal_join: Option<JoinHandle<()>>,
}

#[cfg(not(any(kani, verus)))]
pub(crate) struct CatPresenter {
    states: Option<Arc<CatStates>>,
    last_state: Option<ValidationState>,
    live: Option<LiveHandle>,
}

#[cfg(not(any(kani, verus)))]
impl Drop for CatPresenter {
    fn drop(&mut self) {
        self.stop_live();
    }
}

#[cfg(not(any(kani, verus)))]
impl CatPresenter {
    pub(crate) fn new(no_cat_override: bool) -> Self {
        if no_cat_override || !cat_allowed() {
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

        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Condvar::new());
        let wake_guard = Arc::new(Mutex::new(()));

        #[cfg(unix)]
        let signal_join = match spawn_signal_watcher(Arc::clone(&terminal), Arc::clone(&stop)) {
            Ok(join) => Some(join),
            Err(_) => return false,
        };
        #[cfg(not(unix))]
        let signal_join = None;

        let selection = Arc::new(Mutex::new(
            self.last_state
                .map(LiveSelection::Validation)
                .unwrap_or_else(|| LiveSelection::Named(states.default_state.clone())),
        ));

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
            signal_join,
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
        let _ = stdout.write_all(
            b"
",
        );
        for line in &sprite.lines {
            let _ = stdout.write_all(line.as_bytes());
            let _ = stdout.write_all(
                b"
",
            );
        }
        let _ = stdout.write_all(
            b"
",
        );
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
            render_live_frame(
                &live.terminal,
                self.states.as_deref().unwrap(),
                &selection,
                0,
                true,
            );
            if let Some(join) = live.signal_join {
                let _ = join.join();
            }
            deactivate_terminal(&live.terminal);
        } else {
            self.show_named(final_name);
        }

        self.last_state = Some(final_state);
    }

    pub(crate) fn prepare_for_external_process(&mut self) {
        let Some(mut live) = self.live.take() else {
            return;
        };

        live.stop.store(true, Ordering::Relaxed);
        live.wake.notify_one();
        if let Some(join) = live.join.take() {
            let _ = join.join();
        }
        if let Some(join) = live.signal_join.take() {
            let _ = join.join();
        }

        if let Ok(mut terminal) = live.terminal.lock() {
            let mut cursor = None;
            let _ = terminal.draw(|frame| {
                let area = frame.area();
                frame.render_widget(Clear, area);
                // The external process should start where the inline viewport
                // started, reusing those rows instead of adding blank rows
                // below the cleared cat.
                cursor = Some(Position::new(area.x, area.y));
            });

            if let Some(cursor) = cursor {
                let _ = terminal.set_cursor_position(cursor);
                let _ = terminal.show_cursor();
                let _ = terminal.backend_mut().flush();
            }
        }

        deactivate_terminal(&live.terminal);
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
        if let Some(join) = live.signal_join.take() {
            let _ = join.join();
        }

        deactivate_terminal(&live.terminal);
    }
}

#[cfg(not(any(kani, verus)))]
pub(crate) fn terminal_print(args: fmt::Arguments<'_>, stderr: bool, newline: bool) {
    let mut text = args.to_string();
    if newline {
        text.push('\n');
    }

    if let Some(terminal) = active_terminal() {
        let _ = terminal.lock().map(|mut terminal| {
            let width = terminal.size().map(|area| area.width).unwrap_or(1).max(1);
            let paragraph = Paragraph::new(text.as_str()).wrap(Wrap { trim: false });
            let height = paragraph.line_count(width).max(1).min(u16::MAX as usize) as u16;
            terminal.insert_before(height, |buffer| {
                paragraph.render(buffer.area, buffer);
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

#[cfg(not(any(kani, verus)))]
fn cat_allowed() -> bool {
    cat_allowed_with(
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
        std::env::var_os("GITRUN_NO_CAT").is_some(),
        std::env::var_os("CI").is_some(),
        std::env::var("TERM").ok().as_deref(),
    )
}

#[cfg_attr(verus, verus_verify(dual_spec, open))]
#[cfg_attr(
    verus,
    verus_spec(
        returns stdout_is_terminal && stderr_is_terminal && !no_cat && !ci && !term_is_dumb
    )
)]
pub(crate) fn cat_is_allowed(
    stdout_is_terminal: bool,
    stderr_is_terminal: bool,
    no_cat: bool,
    ci: bool,
    term_is_dumb: bool,
) -> bool {
    stdout_is_terminal && stderr_is_terminal && !no_cat && !ci && !term_is_dumb
}

#[cfg(not(any(kani, verus)))]
fn cat_allowed_with(
    stdout_is_terminal: bool,
    stderr_is_terminal: bool,
    no_cat: bool,
    ci: bool,
    term: Option<&str>,
) -> bool {
    cat_is_allowed(
        stdout_is_terminal,
        stderr_is_terminal,
        no_cat,
        ci,
        term == Some("dumb"),
    )
}
#[cfg(unix)]
#[cfg(not(any(kani, verus)))]
fn spawn_signal_watcher(
    terminal: SharedTerminal,
    stop: Arc<AtomicBool>,
) -> io::Result<JoinHandle<()>> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::flag::register;
    use signal_hook::low_level::unregister;

    let sigint = Arc::new(AtomicBool::new(false));
    let sigterm = Arc::new(AtomicBool::new(false));
    let sigint_id = register(SIGINT, Arc::clone(&sigint))?;
    let sigterm_id = match register(SIGTERM, Arc::clone(&sigterm)) {
        Ok(id) => id,
        Err(error) => {
            unregister(sigint_id);
            return Err(error);
        }
    };

    Ok(thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if sigint.load(Ordering::Relaxed) {
                restore_terminal_cursor(&terminal);
                std::process::exit(130);
            }
            if sigterm.load(Ordering::Relaxed) {
                restore_terminal_cursor(&terminal);
                std::process::exit(143);
            }
            thread::sleep(Duration::from_millis(25));
        }
        unregister(sigint_id);
        unregister(sigterm_id);
    }))
}

#[cfg(not(any(kani, verus)))]
fn restore_terminal_cursor(terminal: &SharedTerminal) {
    if let Ok(mut terminal) = terminal.lock() {
        let _ = terminal.show_cursor();
        let _ = terminal.backend_mut().flush();
    }
}
#[cfg(not(any(kani, verus)))]
fn active_terminal() -> Option<SharedTerminal> {
    ACTIVE_TERMINAL
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|active| active.clone()))
}

#[cfg(not(any(kani, verus)))]
fn activate_terminal(terminal: SharedTerminal) {
    let slot = ACTIVE_TERMINAL.get_or_init(|| Mutex::new(None));
    if let Ok(mut active) = slot.lock() {
        *active = Some(terminal);
    }
}

#[cfg(not(any(kani, verus)))]
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

/// Return a validated ASCII sprite from the canonical cat-state catalogue.
/// The management TUI uses this accessor so both interfaces share the same
/// sprite definitions without reparsing the JSON on each redraw.
#[cfg(not(any(kani, verus)))]
pub(crate) fn cat_state_lines(state_name: &str) -> Option<Vec<String>> {
    static MASCOT_STATES: OnceLock<Option<CatStates>> = OnceLock::new();

    let states = MASCOT_STATES
        .get_or_init(|| {
            serde_json::from_str::<CatStates>(CAT_STATES)
                .ok()
                .filter(validate_states)
        })
        .as_ref()?;
    states
        .states
        .get(state_name)
        .map(|sprite| sprite.lines.clone())
}

#[cfg(not(any(kani, verus)))]
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

#[cfg(not(any(kani, verus)))]
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

#[cfg(not(any(kani, verus)))]
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

#[cfg(not(any(kani, verus)))]
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

    let mut final_cursor = None;
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
            // Frame cursor coordinates are terminal-absolute. Using just the
            // viewport height here would place the shell prompt near the top
            // of the terminal instead of below the inline cat viewport.
            final_cursor = Some(Position::new(
                area.x,
                area.y + area.height.saturating_sub(1),
            ));
        }
    });

    if show_cursor {
        if let Some(cursor) = final_cursor {
            // Put the cursor on the last cat row, advance exactly one row, and
            // then resync Ratatui's cursor tracker with the terminal backend.
            let _ = terminal.set_cursor_position(cursor);
            let _ = terminal.show_cursor();
            let _ = execute!(terminal.backend_mut(), MoveToNextLine(1));
            if let Ok(position) = terminal.get_cursor_position() {
                let _ = terminal.set_cursor_position(position);
            }
        }
    } else {
        let _ = terminal.hide_cursor();
    }
}

#[cfg(not(any(kani, verus)))]
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
    fn management_tui_can_read_validated_cat_sprites() {
        for name in [
            "seated", "curious", "working", "reading", "success", "failed", "recovery",
        ] {
            let lines = cat_state_lines(name).expect("known cat state should exist");
            assert_eq!(lines.len(), 3, "{name}");
            assert!(lines.iter().all(|line| line.is_ascii()));
        }
        assert!(cat_state_lines("not-a-real-state").is_none());
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
    fn cat_is_allowed_only_for_an_interactive_terminal() {
        assert!(cat_allowed_with(true, true, false, false, Some("xterm")));
        assert!(!cat_allowed_with(false, true, false, false, Some("xterm")));
        assert!(!cat_allowed_with(true, false, false, false, Some("xterm")));
        assert!(!cat_allowed_with(true, true, true, false, Some("xterm")));
        assert!(!cat_allowed_with(true, true, false, true, Some("xterm")));
        assert!(!cat_allowed_with(true, true, false, false, Some("dumb")));
        assert!(cat_allowed_with(true, true, false, false, None));
    }

    #[test]
    fn wrapped_terminal_output_height_matches_rendered_paragraph() {
        let paragraph = Paragraph::new("1234567890 1234567890").wrap(Wrap { trim: false });
        assert_eq!(paragraph.line_count(10), 2);
    }

    #[test]
    fn secret_roll_range_is_tiny() {
        assert_eq!(
            (SECRET_ROLL_LIMIT, KEBAB_ROLL_LIMIT, SECRET_ROLL_RANGE),
            (1, 3, 2_000)
        );
    }
}

#[cfg(kani)]
mod kani_verification {
    use super::cat_is_allowed;

    #[kani::proof]
    fn cat_output_is_interactive_only() {
        let stdout_is_terminal: bool = kani::any();
        let stderr_is_terminal: bool = kani::any();
        let no_cat: bool = kani::any();
        let ci: bool = kani::any();
        let term_is_dumb: bool = kani::any();

        let allowed = cat_is_allowed(
            stdout_is_terminal,
            stderr_is_terminal,
            no_cat,
            ci,
            term_is_dumb,
        );
        assert_eq!(
            allowed,
            stdout_is_terminal && stderr_is_terminal && !no_cat && !ci && !term_is_dumb
        );
        assert!(!cat_is_allowed(false, true, false, false, false));
        assert!(!cat_is_allowed(true, false, false, false, false));
        assert!(!cat_is_allowed(true, true, true, false, false));
        assert!(!cat_is_allowed(true, true, false, true, false));
        assert!(!cat_is_allowed(true, true, false, false, true));
        assert!(cat_is_allowed(true, true, false, false, false));
    }
}

#[cfg(verus)]
mod verus_verification {
    use super::cat_is_allowed;
    use vstd::prelude::*;

    verus! {
        proof fn cat_output_is_interactive_only(
            stdout_is_terminal: bool,
            stderr_is_terminal: bool,
            no_cat: bool,
            ci: bool,
            term_is_dumb: bool,
        )
            ensures
                cat_is_allowed(
                    stdout_is_terminal,
                    stderr_is_terminal,
                    no_cat,
                    ci,
                    term_is_dumb,
                )
                    == (stdout_is_terminal && stderr_is_terminal && !no_cat && !ci && !term_is_dumb),
        {
        }
    }
}

#[cfg(kani)]
fn main() {}

#[cfg(verus)]
fn main() {}
