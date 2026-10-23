//! GitRun's interactive, headless management console.
//!
//! This deliberately reuses the dashboard's Tauri-independent command handlers,
//! so settings validation, privileged writes and security gates stay in one place.

use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use serde_json::{json, Value};
use std::{
    env,
    error::Error,
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    process::Command as ProcessCommand,
    time::{SystemTime, UNIX_EPOCH},
};

type ManagementTerminal = Terminal<CrosstermBackend<io::Stdout>>;

const VERSION_FILE: &str = include_str!("../../../version.txt");
const REDACTED: &str = "__GITRUN_KEEP_EXISTING_REDACTED_VALUE__";
const ACCENT: Color = Color::Rgb(177, 245, 103);
const CYAN: Color = Color::Rgb(125, 211, 252);
const MUTED: Color = Color::Rgb(137, 151, 172);
const TEXT: Color = Color::Rgb(232, 238, 247);
const BORDER: Color = Color::Rgb(53, 65, 83);
const GOOD: Color = Color::Rgb(112, 214, 160);
const WARN: Color = Color::Rgb(255, 194, 102);
const BAD: Color = Color::Rgb(255, 116, 128);

#[derive(Clone, Copy)]
struct Page {
    name: &'static str,
    short: &'static str,
    subtitle: &'static str,
    icon: &'static str,
}

const PAGES: &[Page] = &[
    Page {
        name: "Overview",
        short: "Overview",
        subtitle: "Live operational overview and host telemetry.",
        icon: "◈",
    },
    Page {
        name: "Repositories",
        short: "Repositories",
        subtitle: "Configured repositories, runners and repository policy.",
        icon: "⌘",
    },
    Page {
        name: "Runners & Scheduler",
        short: "Runners",
        subtitle: "Runner pool, workflow activity and scheduler configuration.",
        icon: "▤",
    },
    Page {
        name: "Kitty & Resources",
        short: "Kitty",
        subtitle: "Predictive scheduling, adaptive resources and host capacity.",
        icon: "✦",
    },
    Page {
        name: "Privacy & Security",
        short: "Security",
        subtitle: "Trust boundaries, API permissions and runtime hardening.",
        icon: "⬡",
    },
    Page {
        name: "GitVault",
        short: "GitVault",
        subtitle: "Manage secret metadata and scoped secrets without printing values.",
        icon: "▣",
    },
    Page {
        name: "GitSecureRun",
        short: "GSR",
        subtitle: "Runtime protection status, security events and workflow analysis.",
        icon: "⛨",
    },
    Page {
        name: "Logs",
        short: "Logs",
        subtitle: "Structured security events and the systemd service journal.",
        icon: "≋",
    },
    Page {
        name: "Recovery",
        short: "Recovery",
        subtitle: "Host health, repair, update and lifecycle actions.",
        icon: "↻",
    },
    Page {
        name: "General Settings",
        short: "Settings",
        subtitle: "Runner capacity, lifecycle policy, VMs and global configuration.",
        icon: "⚙",
    },
    Page {
        name: "App Settings",
        short: "App settings",
        subtitle: "Service controls, Kitty preferences and operational settings.",
        icon: "◉",
    },
];

#[derive(Clone, Copy)]
struct Action {
    title: &'static str,
    hint: &'static str,
}

const OVERVIEW_ACTIONS: &[Action] = &[
    Action {
        title: "Refresh live overview",
        hint: "Status, runner pool, host resources and repository activity.",
    },
    Action {
        title: "Refresh health checks",
        hint: "Run a fresh read-only check of the local installation.",
    },
    Action {
        title: "Add repository",
        hint: "Connect a repository using the configured GitHub authentication.",
    },
    Action {
        title: "Fast-stop workflow run",
        hint: "Cancel every job in a specified workflow run; requires confirmation.",
    },
];
const REPO_ACTIONS: &[Action] = &[
    Action {
        title: "List configured repositories",
        hint: "Show repository names; select one before opening its details.",
    },
    Action {
        title: "Select / inspect repository",
        hint: "Inspect repository, runner and Logic Containers policy details.",
    },
    Action {
        title: "Connect repository",
        hint: "Run the existing repository connection flow.",
    },
    Action {
        title: "Edit repository policy (JSON)",
        hint: "Open the complete configuration and policy in your editor.",
    },
    Action {
        title: "Inspect API policies",
        hint: "Display the effective API policy summary for the selected repository.",
    },
    Action {
        title: "Inspect GitDockRun requirements",
        hint: "Read workflow requirements detected for the selected repository.",
    },
];
const RUNNER_ACTIONS: &[Action] = &[
    Action {
        title: "Inspect live runner pool",
        hint: "Show runner status and workflow activity from the dashboard backend.",
    },
    Action {
        title: "Refresh host resources",
        hint: "Sample current CPU, memory and disk pressure.",
    },
    Action {
        title: "Inspect runner configuration",
        hint: "Show configured bounds and resource limits, with credentials redacted.",
    },
    Action {
        title: "Run diagnostics",
        hint: "Read a fresh health report without changing the host.",
    },
];
const KITTY_ACTIONS: &[Action] = &[
    Action {
        title: "Inspect Kitty runtime",
        hint: "Read real Kitty settings and runtime status.",
    },
    Action {
        title: "Edit Kitty settings",
        hint: "Edit settings JSON in $VISUAL / $EDITOR; backend validation remains authoritative.",
    },
    Action {
        title: "Inspect host resource budget",
        hint: "Read the most recent CPU, memory and disk sample.",
    },
];
const SECURITY_ACTIONS: &[Action] = &[
    Action {
        title: "Inspect security posture",
        hint: "Show configured controls and runtime verification separately.",
    },
    Action {
        title: "Run security baseline check",
        hint: "Evaluate the same nine configuration/process observations shown by the dashboard.",
    },
    Action {
        title: "Edit runtime security config",
        hint: "Edit a redacted Config document and save through backend validation.",
    },
    Action {
        title: "Edit API/security policy",
        hint: "Edit Config + GitRunSettings with the same write conflict checks as the GUI.",
    },
    Action {
        title: "List API capabilities",
        hint: "Show the closed GitRun API surface.",
    },
];
const VAULT_ACTIONS: &[Action] = &[
    Action {
        title: "List secret metadata",
        hint: "Only names, scopes and timestamps are shown; secret values are never read back.",
    },
    Action {
        title: "Add / update secret",
        hint: "Enter a scope and a value without echoing the value to the terminal.",
    },
    Action {
        title: "Delete secret",
        hint: "Permanently delete one secret after explicit confirmation.",
    },
];
const GSR_ACTIONS: &[Action] = &[
    Action {
        title: "Inspect GSR runtime",
        hint: "Watchdog and protection status are observations, not proof of enforcement.",
    },
    Action {
        title: "Read recent security events",
        hint: "Show structured security events, with no secret values.",
    },
    Action {
        title: "Inspect workflow analyzer",
        hint: "Review zizmor availability, license status and enablement.",
    },
    Action {
        title: "Install / enable zizmor",
        hint: "Requires explicit license and installation confirmation.",
    },
    Action {
        title: "Disable zizmor",
        hint: "Disable the optional workflow analyzer without deleting its binary.",
    },
];
const LOG_ACTIONS: &[Action] = &[
    Action {
        title: "Security events",
        hint: "Read and format the most recent GSR event records.",
    },
    Action {
        title: "Scheduler journal",
        hint: "Read recent gitrun.service output from systemd journal.",
    },
    Action {
        title: "GSR journal",
        hint: "Read recent gitrun-gsr.service output from systemd journal.",
    },
    Action {
        title: "Combined service journal",
        hint: "Read recent logs for both allowlisted GitRun services.",
    },
];
const RECOVERY_ACTIONS: &[Action] = &[
    Action {
        title: "Run host health check",
        hint: "Read-only diagnostics for configuration, Docker and services.",
    },
    Action {
        title: "Check for updates",
        hint: "Inspect the signed release manifest without installing.",
    },
    Action {
        title: "Run terminal setup",
        hint: "Launch the existing first-run/configuration wizard.",
    },
    Action {
        title: "Update GitRun",
        hint: "Download and apply a verified GitRun update after confirmation.",
    },
    Action {
        title: "Update permanent runners",
        hint: "Update/reconcile eligible permanent runner containers after confirmation.",
    },
    Action {
        title: "Reinstall GitRun",
        hint: "Reinstall from the official release after explicit confirmation.",
    },
    Action {
        title: "Restart GitRun service",
        hint: "Restart gitrun.service; active workloads may be interrupted.",
    },
    Action {
        title: "Uninstall GitRun",
        hint: "Destructive: remove GitRun; asks for a second explicit confirmation.",
    },
];
const SETTINGS_ACTIONS: &[Action] = &[
    Action {
        title: "View global configuration",
        hint: "Inspect configuration with known credential-bearing fields redacted.",
    },
    Action {
        title: "Edit global configuration (JSON)",
        hint: "Open all supported configuration fields in $VISUAL / $EDITOR.",
    },
    Action {
        title: "List VM definitions",
        hint: "Read configured virtual-machine backends.",
    },
    Action {
        title: "Add VM definition",
        hint: "Create a VM definition through the backend's validated writer.",
    },
    Action {
        title: "Edit VM definition",
        hint: "Select a VM and edit its structured JSON definition.",
    },
    Action {
        title: "Delete VM definition",
        hint: "Delete a VM definition after confirmation; does not delete its disk.",
    },
    Action {
        title: "Pending hypervisor decisions",
        hint: "View requests that are waiting for an operator choice.",
    },
    Action {
        title: "Resolve hypervisor decision",
        hint: "Choose retry KVM or use VirtualBox for one pending request.",
    },
    Action {
        title: "View Logic Containers rules",
        hint: "Read routing rules and their order.",
    },
    Action {
        title: "Edit Logic Containers rules (JSON)",
        hint: "Edit the full rule list; validated by the scheduler's shared loader.",
    },
];
const APP_ACTIONS: &[Action] = &[
    Action {
        title: "Inspect application health",
        hint: "Current health checks and service state.",
    },
    Action {
        title: "Inspect Kitty settings",
        hint: "Read predictive features, gating and resource bounds.",
    },
    Action {
        title: "Edit Kitty settings (JSON)",
        hint: "Edit Kitty options; all dependency and safety checks remain enforced.",
    },
    Action {
        title: "Start GitRun service",
        hint: "Start gitrun.service through the trusted systemctl elevation path.",
    },
    Action {
        title: "Stop GitRun service",
        hint: "Stop gitrun.service after confirmation.",
    },
    Action {
        title: "Restart GitRun service",
        hint: "Restart gitrun.service after confirmation.",
    },
    Action {
        title: "Inspect repository security settings",
        hint: "Read global security settings and the runtime summary.",
    },
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Pages,
    Actions,
    Details,
}

struct State {
    page: usize,
    action: usize,
    focus: Focus,
    selected_repo: Option<String>,
    body_title: String,
    body: Vec<String>,
    notice: String,
    error: bool,
    scroll: u16,
    show_help: bool,
}

impl State {
    fn new() -> Self {
        Self {
            page: 0,
            action: 0,
            focus: Focus::Actions,
            selected_repo: None,
            body_title: "Welcome to GitRun".into(),
            body: vec![
                "Management console is ready.".into(),
                "Select a section on the left and press Enter to run an operation.".into(),
                "Use Tab to switch between navigation, actions and details.".into(),
                "Live values come from GitRun's real backend; missing values stay unknown.".into(),
            ],
            notice: "Ready".into(),
            error: false,
            scroll: 0,
            show_help: false,
        }
    }

    fn actions(&self) -> &'static [Action] {
        match self.page {
            0 => OVERVIEW_ACTIONS,
            1 => REPO_ACTIONS,
            2 => RUNNER_ACTIONS,
            3 => KITTY_ACTIONS,
            4 => SECURITY_ACTIONS,
            5 => VAULT_ACTIONS,
            6 => GSR_ACTIONS,
            7 => LOG_ACTIONS,
            8 => RECOVERY_ACTIONS,
            9 => SETTINGS_ACTIONS,
            10 => APP_ACTIONS,
            _ => OVERVIEW_ACTIONS,
        }
    }

    fn set_result(&mut self, title: impl Into<String>, value: Value) {
        let title = title.into();
        let mut value = value;
        redact_sensitive(&mut value);
        let formatted = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        self.body_title = title;
        self.body = formatted.lines().map(str::to_owned).collect();
        if self.body.is_empty() {
            self.body.push("(no data returned)".into());
        }
        self.scroll = 0;
        self.error = false;
        self.notice = "Completed · data returned by GitRun backend".into();
    }

    fn set_text(
        &mut self,
        title: impl Into<String>,
        lines: Vec<String>,
        notice: impl Into<String>,
        error: bool,
    ) {
        self.body_title = title.into();
        self.body = if lines.is_empty() {
            vec!["(no output)".into()]
        } else {
            lines
        };
        self.scroll = 0;
        self.notice = notice.into();
        self.error = error;
    }
}

fn app_read(action: &str, args: Value) -> Result<Value, String> {
    gitrun_dashboard_tauri_lib::management_tui_read(action, args)
}

fn app_write(action: &str, args: Value) -> Result<Value, String> {
    gitrun_dashboard_tauri_lib::management_tui_write(action, args)
}

pub(crate) fn run() -> Result<(), Box<dyn Error>> {
    if !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
        || env::var("TERM").ok().as_deref() == Some("dumb")
    {
        return Err("gitrun manage needs an interactive terminal. Over SSH, connect to a TTY or use the direct CLI commands.".into());
    }
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
        let _ = disable_raw_mode();
        return Err(Box::new(error));
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut state = State::new();
    let result = event_loop(&mut terminal, &mut state);
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen, Show);
    let _ = terminal.show_cursor();
    result.map_err(|error| Box::new(io::Error::other(error)) as Box<dyn Error>)
}

fn event_loop(terminal: &mut ManagementTerminal, state: &mut State) -> Result<(), String> {
    loop {
        terminal
            .draw(|frame| render(frame, state))
            .map_err(|error| error.to_string())?;
        match event::read().map_err(|error| error.to_string())? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if state.show_help {
                    state.show_help = false;
                    continue;
                }
                if handle_key(terminal, state, key)? {
                    return Ok(());
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

fn handle_key(
    terminal: &mut ManagementTerminal,
    state: &mut State,
    key: KeyEvent,
) -> Result<bool, String> {
    match key.code {
        KeyCode::Char('q') if key.modifiers.is_empty() => return Ok(true),
        KeyCode::Char('?') => state.show_help = true,
        KeyCode::Tab => {
            state.focus = match state.focus {
                Focus::Pages => Focus::Actions,
                Focus::Actions => Focus::Details,
                Focus::Details => Focus::Pages,
            };
        }
        KeyCode::BackTab => {
            state.focus = match state.focus {
                Focus::Pages => Focus::Details,
                Focus::Actions => Focus::Pages,
                Focus::Details => Focus::Actions,
            };
        }
        KeyCode::Left | KeyCode::Char('h') => {
            state.focus = match state.focus {
                Focus::Pages => Focus::Pages,
                Focus::Actions => Focus::Pages,
                Focus::Details => Focus::Actions,
            };
        }
        KeyCode::Right | KeyCode::Char('l') => {
            state.focus = match state.focus {
                Focus::Pages => Focus::Actions,
                Focus::Actions => Focus::Details,
                Focus::Details => Focus::Details,
            };
        }
        KeyCode::Up | KeyCode::Char('k') => match state.focus {
            Focus::Pages => {
                state.page = state.page.saturating_sub(1);
                state.action = 0;
                state.scroll = 0;
            }
            Focus::Actions => state.action = state.action.saturating_sub(1),
            Focus::Details => state.scroll = state.scroll.saturating_sub(1),
        },
        KeyCode::Down | KeyCode::Char('j') => match state.focus {
            Focus::Pages => {
                state.page = (state.page + 1).min(PAGES.len() - 1);
                state.action = 0;
                state.scroll = 0;
            }
            Focus::Actions => {
                state.action = (state.action + 1).min(state.actions().len().saturating_sub(1))
            }
            Focus::Details => state.scroll = state.scroll.saturating_add(1),
        },
        KeyCode::Home => match state.focus {
            Focus::Pages => {
                state.page = 0;
                state.action = 0;
                state.scroll = 0;
            }
            Focus::Actions => state.action = 0,
            Focus::Details => state.scroll = 0,
        },
        KeyCode::End => match state.focus {
            Focus::Pages => {
                state.page = PAGES.len() - 1;
                state.action = 0;
                state.scroll = 0;
            }
            Focus::Actions => state.action = state.actions().len().saturating_sub(1),
            Focus::Details => {
                state.scroll = state.body.len().saturating_sub(1).min(u16::MAX as usize) as u16
            }
        },
        KeyCode::Enter => {
            if state.focus == Focus::Pages {
                state.focus = Focus::Actions;
                execute_action(terminal, state)?;
            } else if state.focus == Focus::Actions {
                execute_action(terminal, state)?;
            }
        }
        KeyCode::Esc => {
            if state.focus == Focus::Details {
                state.focus = Focus::Actions;
                state.scroll = 0;
            } else {
                state.page = 0;
                state.action = 0;
                state.focus = Focus::Actions;
                execute_action(terminal, state)?;
            }
        }
        KeyCode::Char('r') if key.modifiers.is_empty() => execute_action(terminal, state)?,
        KeyCode::Char(ch @ '1'..='9') if key.modifiers.is_empty() => {
            let page = ch.to_digit(10).unwrap_or(1) as usize - 1;
            if page < PAGES.len() {
                state.page = page;
                state.action = 0;
                state.focus = Focus::Actions;
                execute_action(terminal, state)?;
            }
        }
        KeyCode::Char('0') if key.modifiers.is_empty() => {
            state.page = 10.min(PAGES.len() - 1);
            state.action = 0;
            state.focus = Focus::Actions;
            execute_action(terminal, state)?;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
        _ => {}
    }
    if state.action >= state.actions().len() {
        state.action = state.actions().len().saturating_sub(1);
    }
    Ok(false)
}

fn render(frame: &mut Frame<'_>, state: &State) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(7),
            Constraint::Length(2),
        ])
        .split(area);

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            "  ◈ ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "GitRun",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  /  MANAGEMENT CONSOLE",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            format!("v{}", VERSION_FILE.trim()),
            Style::default().fg(MUTED),
        ),
        Span::raw("   "),
        Span::styled("LOCAL CONTROL PLANE", Style::default().fg(GOOD)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT)),
    );
    frame.render_widget(header, layout[0]);

    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(29),
            Constraint::Percentage(46),
        ])
        .split(layout[1]);
    let page_items: Vec<ListItem<'_>> = PAGES
        .iter()
        .enumerate()
        .map(|(index, page)| {
            let selected = index == state.page;
            let style = if selected {
                Style::default()
                    .fg(Color::Rgb(15, 20, 28))
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", page.icon),
                    if selected {
                        Style::default().fg(Color::Rgb(15, 20, 28))
                    } else {
                        Style::default().fg(CYAN)
                    },
                ),
                Span::styled(page.short, style),
            ]))
        })
        .collect();
    let page_block = Block::default()
        .title(" SECTIONS ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if state.focus == Focus::Pages {
            ACCENT
        } else {
            BORDER
        }));
    let page_list = List::new(page_items)
        .block(page_block)
        .highlight_symbol("› ")
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let mut page_state = ListState::default();
    page_state.select(Some(state.page));
    frame.render_stateful_widget(page_list, middle[0], &mut page_state);

    let actions: Vec<ListItem<'_>> = state
        .actions()
        .iter()
        .enumerate()
        .map(|(index, action)| {
            let selected = index == state.action;
            let style = if selected {
                Style::default()
                    .fg(Color::Rgb(15, 20, 28))
                    .bg(CYAN)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>2} ", index + 1),
                    Style::default().fg(if selected {
                        Color::Rgb(15, 20, 28)
                    } else {
                        MUTED
                    }),
                ),
                Span::styled(action.title, style),
            ]))
        })
        .collect();
    let action_block = Block::default()
        .title(" OPERATIONS ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if state.focus == Focus::Actions {
            CYAN
        } else {
            BORDER
        }));
    let action_list = List::new(actions)
        .block(action_block)
        .highlight_symbol("› ")
        .highlight_style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD));
    let mut action_state = ListState::default();
    action_state.select(Some(state.action));
    frame.render_stateful_widget(action_list, middle[1], &mut action_state);

    let detail_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(1)])
        .split(middle[2]);
    let page = PAGES[state.page];
    let heading = Paragraph::new(vec![
        Line::from(Span::styled(
            state.body_title.as_str(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(page.subtitle, Style::default().fg(MUTED))),
        Line::from(Span::styled(
            state
                .actions()
                .get(state.action)
                .map(|a| a.hint)
                .unwrap_or("Select an operation."),
            Style::default().fg(CYAN),
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if state.focus == Focus::Details {
                ACCENT
            } else {
                BORDER
            })),
    )
    .wrap(Wrap { trim: false });
    frame.render_widget(heading, detail_area[0]);

    let body_text = Text::from(
        state
            .body
            .iter()
            .map(|line| {
                let mut style = Style::default().fg(TEXT);
                if line.starts_with("Error:") {
                    style = style.fg(BAD).add_modifier(Modifier::BOLD);
                } else if line.starts_with("Warning:") || line.contains("\"stale\": true") {
                    style = style.fg(WARN);
                } else if line.starts_with('{')
                    || line.starts_with('}')
                    || line.starts_with('[')
                    || line.starts_with(']')
                {
                    style = style.fg(CYAN);
                }
                Line::from(Span::styled(line.as_str(), style))
            })
            .collect::<Vec<_>>(),
    );
    let body = Paragraph::new(body_text)
        .block(
            Block::default()
                .title(format!(" {} ", PAGES[state.page].name))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(if state.error {
                    BAD
                } else if state.focus == Focus::Details {
                    ACCENT
                } else {
                    BORDER
                })),
        )
        .wrap(Wrap { trim: false })
        .scroll((state.scroll, 0));
    frame.render_widget(body, detail_area[1]);

    let footer = Paragraph::new(Line::from(vec![
        Span::styled(
            " ↑↓ ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Navigate", Style::default().fg(MUTED)),
        Span::raw("  "),
        Span::styled(
            "Enter",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Run  ", Style::default().fg(MUTED)),
        Span::styled(
            "Tab",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Focus  ", Style::default().fg(MUTED)),
        Span::styled("r", Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(" Refresh  ", Style::default().fg(MUTED)),
        Span::styled("?", Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(" Help  ", Style::default().fg(MUTED)),
        Span::styled("q", Style::default().fg(BAD).add_modifier(Modifier::BOLD)),
        Span::styled(" Exit  ", Style::default().fg(MUTED)),
        Span::raw("│ "),
        Span::styled(
            state.notice.as_str(),
            Style::default().fg(if state.error { BAD } else { GOOD }),
        ),
    ]))
    .block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER)),
    );
    frame.render_widget(footer, layout[2]);

    if state.show_help {
        let popup = centered_rect(62, 58, area);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "GitRun management controls",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("↑ / ↓ or j / k    Move the focused list or scroll output"),
                Line::from("Tab / Shift+Tab   Switch between sections, operations, and details"),
                Line::from("← / → or h / l    Move focus"),
                Line::from("Enter             Run the selected operation"),
                Line::from("r                 Refresh the selected operation"),
                Line::from("1–9 / 0           Jump to sections / App settings"),
                Line::from("Esc               Back to operations / Overview"),
                Line::from("q / Ctrl+C        Exit the management console"),
                Line::from(""),
                Line::from(Span::styled(
                    "Press any key to close this help.",
                    Style::default().fg(CYAN),
                )),
            ])
            .block(
                Block::default()
                    .title(" HELP ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(ACCENT)),
            )
            .wrap(Wrap { trim: false }),
            popup,
        );
    }
}

fn centered_rect(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn execute_action(terminal: &mut ManagementTerminal, state: &mut State) -> Result<(), String> {
    state.error = false;
    state.notice = "Working…".into();
    let result: Result<(String, Value), String> = (|| {
        match (state.page, state.action) {
        (0, 0) => {
            let overview = app_read("overview", json!({}))?;
            let health = app_read("health", json!({ "force_refresh": true }))?;
            let runners = app_read("runners", json!({ "force_refresh": true }))?;
            let resources = app_read("host_resources", json!({ "force_refresh": true }))?;
            let activity = app_read("repository_activity", json!({ "force_refresh": true }))?;
            Ok(("Live overview".into(), json!({ "overview": overview, "health": health, "runners": runners, "host_resources": resources, "repository_activity": activity })))
        }
        (0, 1) | (2, 3) | (8, 0) | (10, 0) => {
            app_read("health", json!({ "force_refresh": true })).map(|value| ("Host health".into(), value))
        }
        (0, 2) | (1, 2) => connect_repository(terminal, state).map(|value| ("Repository connection".into(), value)),
        (0, 3) => fast_stop(terminal).map(|value| ("Fast Stop".into(), value)),
        (1, 0) => app_read("config", json!({})).map(|value| ("Configured repositories".into(), json!({ "repositories": value.get("repositories").cloned().unwrap_or(Value::Null), "selected_repository": state.selected_repo }))),
        (1, 1) => inspect_repository(terminal, state),
        (1, 3) => edit_repository_policy(terminal, state).map(|value| ("Repository policy".into(), value)),
        (1, 4) => match selected_repo(state) { Some(repo) => app_read("repo_api_policies", json!({ "repo": repo })).map(|v| ("Repository API policies".into(),v)), None => Err("Select a repository first.".into()) },
        (1, 5) => match selected_repo(state) { Some(repo) => app_read("dock_requirements", json!({ "repo": repo })).map(|v| ("GitDockRun requirements".into(),v)), None => Err("Select a repository first.".into()) },
        (2, 0) => app_read("runners", json!({ "force_refresh": true })).map(|v| ("Live runner pool".into(),v)),
        (2, 1) => app_read("host_resources", json!({ "force_refresh": true })).map(|v| ("Host resource snapshot".into(),v)),
        (2, 2) => app_read("config", json!({})).map(|v| ("Runner configuration".into(),v)),
        (3, 0) => app_read("kitty", json!({})).map(|v| ("Kitty runtime".into(),v)),
        (3, 1) => edit_kitty(terminal).map(|v| ("Kitty settings saved".into(),v)),
        (3, 2) => app_read("host_resources", json!({ "force_refresh": true })).map(|v| ("Host resources".into(),v)),
        (4, 0) => {
            let config = app_read("config", json!({}))?;
            let settings = app_read("gitrun_settings", json!({}))?;
            let summary = app_read("security_summary", json!({}))?;
            Ok(("Security posture".into(), json!({ "configuration": config, "policy": settings, "runtime": summary })))
        }
        (4, 1) => security_baseline_check().map(|v| ("Security baseline".into(),v)),
        (4, 2) => edit_config(terminal).map(|v| ("Runtime security configuration".into(),v)),
        (4, 3) => edit_security_policy(terminal).map(|v| ("API and security policy".into(),v)),
        (4, 4) => run_external(terminal, &["--no-cat", "api-list"]).map(|v| ("GitRun API capabilities".into(),v)),
        (5, 0) => app_read("vault_secrets", json!({})).map(|v| ("GitVault secret metadata".into(),v)),
        (5, 1) => add_secret(terminal).map(|v| ("GitVault secret".into(),v)),
        (5, 2) => delete_secret(terminal).map(|v| ("GitVault delete".into(),v)),
        (6, 0) => {
            let status = app_read("gsr_status", json!({}))?;
            let zizmor = app_read("zizmor", json!({}))?;
            Ok(("GitSecureRun status".into(),json!({ "status": status, "zizmor": zizmor })))
        }
        (6, 1) => app_read("gsr_events", json!({ "limit": 30 })).map(|v| ("Recent security events".into(),v)),
        (6, 2) => app_read("zizmor", json!({})).map(|v| ("Workflow analyzer status".into(),v)),
        (6, 3) => {
            if confirm(terminal, "Install optional zizmor analyzer and accept its license? [y/N]")? {
                app_write("zizmor_install", json!({})).map(|v| ("zizmor installation".into(),v))
            } else { Ok(("Installation cancelled".into(),json!({"cancelled":true}))) }
        }
        (6, 4) => {
            if confirm(terminal, "Disable the optional zizmor analyzer? [y/N]")? {
                app_write("zizmor_disable", json!({})).map(|v| ("zizmor".into(),v))
            } else { Ok(("Action cancelled".into(),json!({"cancelled":true}))) }
        }
        (7, 0) => app_read("gsr_events", json!({ "limit": 100 })).map(|v| ("Security event log".into(),v)),
        (7, 1) => app_read("service_logs", json!({ "source":"scheduler", "limit":200 })).map(|v| ("Scheduler journal".into(),v)),
        (7, 2) => app_read("service_logs", json!({ "source":"gsr", "limit":200 })).map(|v| ("GSR journal".into(),v)),
        (7, 3) => app_read("service_logs", json!({ "source":"all", "limit":200 })).map(|v| ("Combined service journal".into(),v)),
        (8, 1) => app_read("updates", json!({})).map(|v| ("Update check".into(),v)),
        (8, 2) => run_external(terminal, &["--no-cat","setup","--terminal"]).map(|v| ("Terminal setup".into(),v)),
        (8, 3) => external_with_confirmation(terminal, &["--no-cat","update"], "Apply the verified GitRun update now? [y/N]", "GitRun update"),
        (8, 4) => external_with_confirmation(terminal, &["--no-cat","update","--only-containers"], "Update eligible permanent runner containers now? [y/N]", "Permanent runner update"),
        (8, 5) => external_with_confirmation(terminal, &["--no-cat","reinstall"], "Reinstall GitRun from the official release? [y/N]", "GitRun reinstall"),
        (8, 6) => service_action(terminal, "restart").map(|v| ("GitRun service".into(), v)),
        (8, 7) => external_with_confirmation(terminal, &["--no-cat","uninstall"], "UNINSTALL GitRun and its managed components? [y/N]", "GitRun uninstall"),
        (9, 0) => app_read("config", json!({})).map(|v| ("Global configuration".into(),v)),
        (9, 1) => edit_config(terminal).map(|v| ("Global configuration".into(),v)),
        (9, 2) => app_read("vm_configs", json!({})).map(|v| ("VM definitions".into(),v)),
        (9, 3) => add_vm(terminal).map(|v| ("VM definition".into(),v)),
        (9, 4) => edit_vm(terminal).map(|v| ("VM definition".into(),v)),
        (9, 5) => delete_vm(terminal).map(|v| ("VM definition".into(),v)),
        (9, 6) => app_read("hypervisor_decisions", json!({})).map(|v| ("Pending hypervisor decisions".into(),v)),
        (9, 7) => resolve_hypervisor_decision(terminal).map(|v| ("Hypervisor decision".into(),v)),
        (9, 8) => app_read("logic_rules", json!({})).map(|v| ("Logic Containers rules".into(),v)),
        (9, 9) => edit_logic_rules(terminal).map(|v| ("Logic Containers rules".into(),v)),
        (10, 1) => app_read("kitty", json!({})).map(|v| ("Kitty settings".into(),v)),
        (10, 2) => edit_kitty(terminal).map(|v| ("Kitty settings saved".into(),v)),
        (10, 3) => service_action(terminal, "start").map(|v| ("GitRun service".into(),v)),
        (10, 4) => {
            if confirm(terminal, "Stop gitrun.service? Active workloads may be interrupted. [y/N]")? {
                service_control("stop").map(|v| ("GitRun service".into(),v))
            } else { Ok(("Action cancelled".into(),json!({"cancelled":true}))) }
        }
        (10, 5) => service_action(terminal, "restart").map(|v| ("GitRun service".into(),v)),
        (10, 6) => {
            let config = app_read("config", json!({}))?;
            let settings = app_read("gitrun_settings", json!({}))?;
            let runtime = app_read("security_summary", json!({}))?;
            Ok(("Repository security settings".into(),json!({"config":config,"settings":settings,"runtime":runtime})))
        }
        _ => Err("This operation is not implemented for the selected section.".into()),
    }
    })();

    match result {
        Ok((title, value)) => state.set_result(title, value),
        Err(error) => state.set_text(
            "Operation failed",
            vec![
                format!("Error: {error}"),
                "No successful result was reported.".into(),
            ],
            "Operation failed",
            true,
        ),
    }
    Ok(())
}

fn security_baseline_check() -> Result<Value, String> {
    let config = app_read("config", json!({}))?;
    let settings = app_read("gitrun_settings", json!({}))?;
    let gsr = app_read("gsr_status", json!({}))?;

    let network = config.get("runner_network").and_then(Value::as_str).unwrap_or("");
    let unsafe_runner = config
        .get("gsr_allow_unsafe_runner")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let hardening = config
        .get("gsr_docker_socket_hardening")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let direct_socket_repos = settings
        .get("repositories")
        .and_then(Value::as_object)
        .map(|repos| {
            repos
                .iter()
                .filter_map(|(name, value)| {
                    let enabled = value
                        .get("docker")
                        .and_then(|docker| docker.get("direct_socket_enabled"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    enabled.then(|| name.clone())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let cache_scope = config
        .get("shared_cache_scope")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    let checks = vec![
        json!({
            "label":"Runner network is dedicated",
            "ok":!network.is_empty() && network!="bridge" && network!="host" && !network.starts_with("container:"),
            "detail":format!("Configured network: {}",if network.is_empty() {"not set"} else {network})
        }),
        json!({
            "label":"Read-only runner root filesystem",
            "ok":config.get("runner_rootfs_read_only").and_then(Value::as_bool).unwrap_or(false),
            "detail":"Configured root filesystem posture"
        }),
        json!({
            "label":"Docker socket hardening",
            "ok":hardening && !unsafe_runner,
            "detail":format!("Hardening {}; unsafe gate {}",if hardening {"enabled"} else {"disabled"},if unsafe_runner {"enabled"} else {"disabled"})
        }),
        json!({
            "label":"GSR command policy and baseline blacklist",
            "ok":config.get("gsr_command_policy_enabled").and_then(Value::as_bool).unwrap_or(false)
                && config.get("gsr_command_baseline_blacklist_enabled").and_then(Value::as_bool).unwrap_or(false),
            "detail":"Built-in command policy configuration"
        }),
        json!({
            "label":"Workflow validation",
            "ok":config.get("gsr_workflow_validation_enabled").and_then(Value::as_bool).unwrap_or(false),
            "detail":"Pre-admission validation configuration"
        }),
        json!({
            "label":"Resource-pressure backpressure",
            "ok":config.get("resource_pressure_enabled").and_then(Value::as_bool).unwrap_or(false),
            "detail":"Host pressure thresholds configured"
        }),
        json!({
            "label":"Cache scope is not global",
            "ok":cache_scope!="global",
            "detail":format!("Configured cache scope: {cache_scope}")
        }),
        json!({
            "label":"No direct socket exceptions",
            "ok":direct_socket_repos.is_empty(),
            "detail":if direct_socket_repos.is_empty() {"No repository-specific direct socket exceptions".to_owned()} else {direct_socket_repos.join(", ")}
        }),
        json!({
            "label":"GSR process observed",
            "ok":gsr.get("watching").and_then(Value::as_bool).unwrap_or(false),
            "detail":if gsr.get("watching").and_then(Value::as_bool).unwrap_or(false) {
                "Process-name observation succeeded; enforcement is still not verified.".to_owned()
            } else {
                "The expected GSR process name was not detected.".to_owned()
            }
        }),
    ];
    let passed = checks
        .iter()
        .filter(|check| check.get("ok").and_then(Value::as_bool) == Some(true))
        .count();
    Ok(json!({
        "passed":passed,
        "total":checks.len(),
        "checks":checks,
        "interpretation":"This is a policy-configuration and process-presence check, not end-to-end proof of runtime enforcement."
    }))
}

fn selected_repo(state: &State) -> Option<String> {
    state.selected_repo.clone()
}

fn inspect_repository(
    terminal: &mut ManagementTerminal,
    state: &mut State,
) -> Result<(String, Value), String> {
    let config = app_read("config", json!({}))?;
    let repos = config
        .get("repositories")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    if repos.is_empty() {
        return Err("No repositories are configured. Use Connect repository first.".into());
    }
    let labels = repos.clone();
    let selected = prompt_number(
        terminal,
        "Select configured repository",
        &labels,
        repos.len(),
    )?;
    let repo = repos[selected].clone();
    state.selected_repo = Some(repo.clone());
    let detail = app_read("repo_detail", json!({"repo":repo}))?;
    let bootstrap = app_read("repo_bootstrap", json!({"repo":repo}))?;
    Ok((
        format!("Repository: {repo}"),
        json!({"repository":detail,"policy_bootstrap":bootstrap}),
    ))
}

fn connect_repository(
    terminal: &mut ManagementTerminal,
    _state: &mut State,
) -> Result<Value, String> {
    let repo = prompt(terminal, "Repository (owner/name)")?;
    if !valid_repo(&repo) {
        return Err(
            "Repository must be in owner/name form and contain only valid GitHub name characters."
                .into(),
        );
    }
    let args = vec!["--no-cat".to_owned(), "connect".to_owned(), repo.clone()];
    let output = run_external_args(terminal, &args)?;
    if output.get("exit_code").and_then(Value::as_i64) == Some(0) {
        Ok(output)
    } else {
        Err(format!(
            "Connect command did not complete successfully: {}",
            output
        ))
    }
}

fn fast_stop(terminal: &mut ManagementTerminal) -> Result<Value, String> {
    let repo = prompt(terminal, "Repository (owner/name)")?;
    if !valid_repo(&repo) {
        return Err("Repository must be in owner/name form.".into());
    }
    let run_id = prompt(terminal, "Workflow run ID")?
        .parse::<u64>()
        .map_err(|_| "Workflow run ID must be an unsigned integer.".to_owned())?;
    if run_id == 0 {
        return Err("Workflow run ID must be greater than zero.".into());
    }
    if !confirm(
        terminal,
        &format!(
            "Cancel every job in workflow run {run_id} for {repo}? This cannot be undone. [y/N]"
        ),
    )? {
        return Ok(json!({"cancelled":true}));
    }
    app_write("fast_stop", json!({"repository":repo,"run_id":run_id}))
}

fn edit_repository_policy(
    terminal: &mut ManagementTerminal,
    state: &State,
) -> Result<Value, String> {
    let config = app_read("config", json!({}))?;
    let settings = app_read("gitrun_settings", json!({}))?;
    let repo = selected_repo(state).ok_or("Select a repository first.")?;
    let mut document = json!({
        "_instructions": format!("Edit config/settings. Selected repository context: {}. Remove _instructions before saving or leave it unchanged; it will be stripped.", repo),
        "config": config.clone(),
        "settings": settings.clone()
    });
    redact_sensitive(&mut document);
    let edited = edit_json_document(terminal, &document)?;
    let mut edited = edited;
    if let Some(object) = edited.as_object_mut() {
        object.remove("_instructions");
    }
    restore_redacted(
        &mut edited,
        &json!({"config":config.clone(),"settings":settings.clone()}),
    );
    let updated = edited
        .get("config")
        .cloned()
        .ok_or("edited document needs a config object")?;
    let next_settings = edited
        .get("settings")
        .cloned()
        .ok_or("edited document needs a settings object")?;
    app_write(
        "dashboard_settings_save",
        json!({
            "updated":updated,
            "settings":next_settings,
            "confirm_socket_opt_out":confirm(terminal,"Confirm any newly enabled direct Docker socket access? [y/N]")?,
            "expected_config":config,
            "expected_settings":settings
        }),
    )?;
    Ok(json!({"saved":true,"selected_repository":repo}))
}

fn edit_config(terminal: &mut ManagementTerminal) -> Result<Value, String> {
    let config = app_read("config", json!({}))?;
    let mut edited = redact_copy(config.clone());
    let value = edit_json_document(terminal, &edited)?;
    edited = value;
    restore_redacted(&mut edited, &config);
    app_write("config_save", json!({"updated":edited}))?;
    Ok(json!({"saved":true,"message":"Configuration accepted by the backend."}))
}

fn edit_security_policy(terminal: &mut ManagementTerminal) -> Result<Value, String> {
    let config = app_read("config", json!({}))?;
    let settings = app_read("gitrun_settings", json!({}))?;
    let mut document = redact_copy(json!({"config":config.clone(),"settings":settings.clone()}));
    let edited = edit_json_document(terminal, &document)?;
    document = edited;
    restore_redacted(
        &mut document,
        &json!({"config":config.clone(),"settings":settings.clone()}),
    );
    let changed = document
        .get("settings")
        .and_then(|v| v.get("repositories"))
        .is_some();
    let confirm_socket = if changed {
        confirm(
            terminal,
            "Confirm newly enabled direct Docker socket access (if any)? [y/N]",
        )?
    } else {
        false
    };
    app_write(
        "dashboard_settings_save",
        json!({
            "updated":document.get("config").cloned().ok_or("missing config")?,
            "settings":document.get("settings").cloned().ok_or("missing settings")?,
            "confirm_socket_opt_out":confirm_socket,
            "expected_config":config,
            "expected_settings":settings
        }),
    )?;
    Ok(json!({"saved":true}))
}

fn edit_kitty(terminal: &mut ManagementTerminal) -> Result<Value, String> {
    let original = app_read("kitty", json!({}))?;
    let settings = original
        .get("settings")
        .cloned()
        .ok_or("Kitty response did not contain settings")?;
    let mut edited = edit_json_document(terminal, &redact_copy(settings.clone()))?;
    restore_redacted(&mut edited, &settings);
    app_write("kitty_save", json!({"settings":edited}))?;
    app_read("kitty", json!({}))
}

fn edit_logic_rules(terminal: &mut ManagementTerminal) -> Result<Value, String> {
    let rules = app_read("logic_rules", json!({}))?;
    let mut edited = edit_json_document(terminal, &rules)?;
    if !edited.is_array() {
        return Err("Logic Containers rule document must remain a JSON array.".into());
    }
    app_write("logic_rules_save", json!({"rules":edited}))?;
    app_read("logic_rules", json!({}))
}

fn add_secret(terminal: &mut ManagementTerminal) -> Result<Value, String> {
    let name = prompt(terminal, "Secret name")?;
    if name.trim().is_empty() {
        return Err("Secret name cannot be empty.".into());
    }
    let scope = prompt_number(
        terminal,
        "Secret scope",
        &[
            "Global".into(),
            "Group (share within a GitVault group)".into(),
            "Repository (owner/name)".into(),
        ],
        3,
    )?;
    let scope_value = match scope {
        0 => json!({"kind":"Global"}),
        1 => {
            let group = prompt(terminal, "GitVault group name")?;
            if group.trim().is_empty() {
                return Err("Group name cannot be empty.".into());
            }
            json!({"kind":"Group","value":group})
        }
        _ => {
            let repo = prompt(terminal, "Repository scope (owner/name)")?;
            if !valid_repo(&repo) {
                return Err("Repository scope must be owner/name.".into());
            }
            json!({"kind":"Repo","value":repo})
        }
    };
    let secret = prompt_secret(terminal, "Secret value (input is hidden)")?;
    if secret.is_empty() {
        return Err("Secret value cannot be empty.".into());
    }
    app_write(
        "vault_set",
        json!({"name":name,"value":secret,"scope":scope_value}),
    )?;
    Ok(json!({"saved":true,"name":name,"scope":scope_value,"value":"[not displayed]"}))
}

fn delete_secret(terminal: &mut ManagementTerminal) -> Result<Value, String> {
    let secrets = app_read("vault_secrets", json!({}))?;
    let items = secrets.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return Ok(json!({"message":"No GitVault secrets are configured."}));
    }
    let labels = items
        .iter()
        .map(|v| {
            format!(
                "{} · {}",
                v.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                scope_label(v.get("scope"))
            )
        })
        .collect::<Vec<_>>();
    let index = prompt_number(terminal, "Select secret to delete", &labels, items.len())?;
    let secret = &items[index];
    let name = secret
        .get("name")
        .and_then(Value::as_str)
        .ok_or("secret entry has no name")?;
    let scope = secret
        .get("scope")
        .cloned()
        .ok_or("secret entry has no scope")?;
    if !confirm(
        terminal,
        &format!(
            "Permanently delete secret metadata/value for {name} ({})? [y/N]",
            scope_label(Some(&scope))
        ),
    )? {
        return Ok(json!({"cancelled":true}));
    }
    app_write("vault_delete", json!({"name":name,"scope":scope}))
}

fn scope_label(scope: Option<&Value>) -> String {
    let Some(scope) = scope else {
        return "unknown scope".into();
    };
    match scope.get("kind").and_then(Value::as_str) {
        Some("Global") => "global".into(),
        Some("Group") => format!(
            "group {}",
            scope.get("value").and_then(Value::as_str).unwrap_or("?")
        ),
        Some("Repo") => format!(
            "repo {}",
            scope.get("value").and_then(Value::as_str).unwrap_or("?")
        ),
        _ => scope.to_string(),
    }
}

fn resolve_hypervisor_decision(terminal: &mut ManagementTerminal) -> Result<Value, String> {
    let decisions = app_read("hypervisor_decisions", json!({}))?;
    let items = decisions.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return Ok(json!({"message":"No pending hypervisor decisions are waiting."}));
    }
    let labels = items
        .iter()
        .map(|item| {
            format!(
                "{} · {}",
                item.get("vm_name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown VM"),
                item.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("no error detail")
            )
        })
        .collect::<Vec<_>>();
    let index = prompt_number(terminal, "Select pending VM decision", &labels, items.len())?;
    let vm_name = items[index]
        .get("vm_name")
        .and_then(Value::as_str)
        .ok_or("pending decision has no VM name")?;
    let choice_index = prompt_number(
        terminal,
        &format!("Choose action for {vm_name}"),
        &["Retry KVM".into(), "Use VirtualBox".into()],
        2,
    )?;
    let choice = if choice_index == 0 {
        "retry_kvm"
    } else {
        "use_virtual_box"
    };
    if !confirm(
        terminal,
        &format!("Record {choice} for {vm_name}? The scheduler will perform the operation. [y/N]"),
    )? {
        return Ok(json!({"cancelled":true}));
    }
    app_write(
        "hypervisor_respond",
        json!({"vm_name":vm_name,"choice":choice}),
    )
}

fn add_vm(terminal: &mut ManagementTerminal) -> Result<Value, String> {
    let name = prompt(terminal, "VM name")?;
    let hypervisor = prompt_number(
        terminal,
        "Hypervisor",
        &["KVM".into(), "VirtualBox".into()],
        2,
    )?;
    let base_disk_image = prompt(terminal, "Base disk image path")?;
    let memory_mb = prompt(terminal, "Memory in MiB")?
        .parse::<u32>()
        .map_err(|_| "Memory must be a positive integer".to_owned())?;
    let cpus = prompt(terminal, "Virtual CPUs")?
        .parse::<u32>()
        .map_err(|_| "CPU count must be a positive integer".to_owned())?;
    let docker_port = prompt(terminal, "Docker TCP port")?
        .parse::<u16>()
        .map_err(|_| "Docker port must be an integer from 1 to 65535".to_owned())?;
    let docker_tls_cert_dir = prompt(terminal, "Docker TLS certificate directory")?;
    let activation = prompt_number(
        terminal,
        "Activation mode",
        &["Standard".into(), "AlwaysOnExperimental".into()],
        2,
    )?;
    let is_windows = confirm(terminal, "Is this a Windows guest? [y/N]")?;
    let entry = json!({
        "name":name,
        "hypervisor":if hypervisor==0 {"Kvm"} else {"VirtualBox"},
        "base_disk_image":base_disk_image,
        "memory_mb":memory_mb,
        "cpus":cpus,
        "docker_port":docker_port,
        "docker_tls_cert_dir":docker_tls_cert_dir,
        "activation":if activation==0 {"Standard"} else {"AlwaysOnExperimental"},
        "is_windows":is_windows
    });
    app_write("vm_save", json!({"config_entry":entry}))?;
    app_read("vm_configs", json!({}))
}

fn edit_vm(terminal: &mut ManagementTerminal) -> Result<Value, String> {
    let vms = app_read("vm_configs", json!({}))?;
    let items = vms.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return Err("No VM definitions exist yet. Use Add VM definition first.".into());
    }
    let labels = items
        .iter()
        .map(|v| {
            v.get("name")
                .and_then(Value::as_str)
                .unwrap_or("unnamed")
                .to_owned()
        })
        .collect::<Vec<_>>();
    let index = prompt_number(terminal, "Select VM to edit", &labels, items.len())?;
    let edited = edit_json_document(terminal, &redact_copy(items[index].clone()))?;
    if edited.get("name") != items[index].get("name") {
        return Err("VM names are identity keys; editing cannot rename a VM. Delete and recreate it instead.".into());
    }
    app_write("vm_save", json!({"config_entry":edited}))?;
    app_read("vm_configs", json!({}))
}

fn delete_vm(terminal: &mut ManagementTerminal) -> Result<Value, String> {
    let vms = app_read("vm_configs", json!({}))?;
    let items = vms.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return Ok(json!({"message":"No VM definitions are configured."}));
    }
    let labels = items
        .iter()
        .enumerate()
        .map(|(i, v)| {
            format!(
                "{}: {}",
                i + 1,
                v.get("name").and_then(Value::as_str).unwrap_or("unnamed")
            )
        })
        .collect::<Vec<_>>();
    let index = prompt_number(
        terminal,
        "Select VM definition to remove",
        &labels,
        items.len(),
    )?;
    let name = items[index]
        .get("name")
        .and_then(Value::as_str)
        .ok_or("VM has no name")?;
    if !confirm(terminal,&format!("Remove VM definition {name}? Its disk and hypervisor resources will not be deleted. [y/N]"))? {return Ok(json!({"cancelled":true}));}
    app_write("vm_delete", json!({"name":name}))?;
    app_read("vm_configs", json!({}))
}

fn service_action(terminal: &mut ManagementTerminal, action: &str) -> Result<Value, String> {
    let prompt_text = match action {
        "stop" => Some("Stop the GitRun service? Active workloads may be interrupted. [y/N]"),
        "restart" => Some("Restart gitrun.service? Active workloads may be interrupted. [y/N]"),
        _ => None,
    };
    if let Some(prompt_text) = prompt_text {
        if !confirm(terminal, prompt_text)? {
            return Ok(json!({"cancelled":true}));
        }
    }
    service_control(action)
}

fn service_control(action: &str) -> Result<Value, String> {
    app_write("service_control", json!({"action":action}))
}

fn external_with_confirmation(
    terminal: &mut ManagementTerminal,
    args: &[&str],
    prompt_text: &str,
    title: &str,
) -> Result<(String, Value), String> {
    if !confirm(terminal, prompt_text)? {
        return Ok((format!("{title} cancelled"), json!({"cancelled":true})));
    }
    run_external(terminal, args).map(|v| (title.to_owned(), v))
}

fn run_external(terminal: &mut ManagementTerminal, args: &[&str]) -> Result<Value, String> {
    run_external_args(
        terminal,
        &args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>(),
    )
}

fn run_external_args(terminal: &mut ManagementTerminal, args: &[String]) -> Result<Value, String> {
    let executable =
        env::current_exe().map_err(|e| format!("cannot locate gitrun executable: {e}"))?;
    let captured = suspend_terminal(terminal, || {
        let status = ProcessCommand::new(executable)
            .args(args)
            .status()
            .map_err(|e| format!("unable to launch GitRun command: {e}"))?;
        println!("\nGitRun command exited with {status}.");
        println!("Press Enter to return to the management console.");
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
        Ok(json!({"exit_code":status.code().unwrap_or(1),"success":status.success(),"args":args}))
    })?;
    Ok(captured)
}

fn prompt(terminal: &mut ManagementTerminal, label: &str) -> Result<String, String> {
    suspend_terminal(terminal, || {
        print!("{label}: ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        let mut value = String::new();
        io::stdin()
            .read_line(&mut value)
            .map_err(|e| e.to_string())?;
        Ok(value.trim_end_matches(['\r', '\n']).to_owned())
    })
}

#[cfg(unix)]
struct TerminalEchoGuard;

#[cfg(unix)]
impl Drop for TerminalEchoGuard {
    fn drop(&mut self) {
        let _ = ProcessCommand::new("stty").arg("echo").status();
        println!();
    }
}

fn prompt_secret(terminal: &mut ManagementTerminal, label: &str) -> Result<String, String> {
    #[cfg(unix)]
    {
        suspend_terminal(terminal, || {
            let disabled = ProcessCommand::new("stty")
                .arg("-echo")
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !disabled {
                return Err(
                    "could not disable terminal echo; refusing to accept a secret visibly".into(),
                );
            }
            let _echo_guard = TerminalEchoGuard;
            print!("{label}: ");
            io::stdout().flush().map_err(|e| e.to_string())?;
            let mut value = String::new();
            io::stdin()
                .read_line(&mut value)
                .map_err(|e| e.to_string())?;
            Ok(value.trim_end_matches(['\r', '\n']).to_owned())
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (terminal, label);
        Err("hidden secret input is supported only on Unix terminals".into())
    }
}

fn prompt_number(
    terminal: &mut ManagementTerminal,
    title: &str,
    choices: &[String],
    max: usize,
) -> Result<usize, String> {
    suspend_terminal(terminal, || {
        println!("\n{title}");
        for (index, choice) in choices.iter().enumerate() {
            println!("  {}. {choice}", index + 1);
        }
        loop {
            print!("Select 1-{} (0 cancels): ", max);
            io::stdout().flush().map_err(|e| e.to_string())?;
            let mut input = String::new();
            io::stdin()
                .read_line(&mut input)
                .map_err(|e| e.to_string())?;
            match input.trim().parse::<usize>() {
                Ok(0) => return Err("Selection cancelled.".into()),
                Ok(value) if (1..=max).contains(&value) => return Ok(value - 1),
                _ => println!("Enter a number from 1 to {max}, or 0 to cancel."),
            }
        }
    })
}

fn confirm(terminal: &mut ManagementTerminal, label: &str) -> Result<bool, String> {
    suspend_terminal(terminal, || {
        print!("{label}: ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(|e| e.to_string())?;
        Ok(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    })
}

fn edit_json_document(terminal: &mut ManagementTerminal, value: &Value) -> Result<Value, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let mut path = PathBuf::from(env::temp_dir());
    path.push(format!("gitrun-manage-{}-{now}.json", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let content = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    {
        let mut file = options
            .open(&path)
            .map_err(|e| format!("unable to create private editor file: {e}"))?;
        file.write_all(&content).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    let result = suspend_terminal(terminal, || {
        let editor = env::var("VISUAL")
            .or_else(|_| env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".into());
        let mut parts = editor.split_whitespace();
        let program = parts.next().ok_or("VISUAL/EDITOR is empty")?;
        let status = ProcessCommand::new(program)
            .args(parts)
            .arg(&path)
            .status()
            .map_err(|e| format!("could not launch {program}; set VISUAL or EDITOR: {e}"))?;
        if !status.success() {
            return Err(format!("editor exited with {status}"));
        }
        let mut bytes = Vec::new();
        fs::File::open(&path)
            .map_err(|e| e.to_string())?
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        serde_json::from_slice(&bytes).map_err(|e| format!("edited JSON is invalid: {e}"))
    });
    let _ = fs::remove_file(&path);
    result
}

fn suspend_terminal<T>(
    terminal: &mut ManagementTerminal,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    disable_raw_mode().map_err(|e| format!("cannot suspend terminal mode: {e}"))?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, Show)
        .map_err(|e| format!("cannot leave management screen: {e}"))?;
    let result = operation();
    enable_raw_mode().map_err(|e| format!("cannot restore terminal input mode: {e}"))?;
    execute!(terminal.backend_mut(), EnterAlternateScreen, Hide)
        .map_err(|e| format!("cannot restore management screen: {e}"))?;
    terminal.clear().map_err(|e| e.to_string())?;
    result
}

fn redact_copy(mut value: Value) -> Value {
    redact_sensitive(&mut value);
    value
}

fn redact_sensitive(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let key_lower = key.to_ascii_lowercase();
                if [
                    "token",
                    "secret",
                    "password",
                    "private_key",
                    "privatekey",
                    "credential",
                    "pat",
                ]
                .iter()
                .any(|needle| key_lower.contains(needle))
                {
                    if !child.is_null() {
                        *child = Value::String(REDACTED.into());
                    }
                } else {
                    redact_sensitive(child);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_sensitive(item);
            }
        }
        _ => {}
    }
}

fn restore_redacted(edited: &mut Value, original: &Value) {
    match (edited, original) {
        (Value::Object(edited_map), Value::Object(original_map)) => {
            for (key, value) in edited_map.iter_mut() {
                if value.as_str() == Some(REDACTED) {
                    if let Some(previous) = original_map.get(key) {
                        *value = previous.clone();
                    }
                } else if let Some(previous) = original_map.get(key) {
                    restore_redacted(value, previous);
                }
            }
        }
        (Value::Array(edited_items), Value::Array(original_items)) => {
            for (edited, original) in edited_items.iter_mut().zip(original_items.iter()) {
                restore_redacted(edited, original);
            }
        }
        _ => {}
    }
}

fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let Some(owner) = parts.next() else {
        return false;
    };
    let Some(name) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && !owner.is_empty()
        && !name.is_empty()
        && owner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_management_page_has_actions() {
        assert_eq!(PAGES.len(), 11);
        for index in 0..PAGES.len() {
            let state = State {
                page: index,
                ..State::new()
            };
            assert!(
                !state.actions().is_empty(),
                "missing actions for {}",
                PAGES[index].name
            );
        }
    }

    #[test]
    fn repository_input_requires_owner_and_name() {
        assert!(valid_repo("Vider06/GitRun"));
        assert!(!valid_repo("GitRun"));
        assert!(!valid_repo("owner/repo/extra"));
        assert!(!valid_repo("/repo"));
        assert!(!valid_repo("owner/"));
    }

    #[test]
    fn redaction_never_prints_sensitive_config_values() {
        let secret = json!({"github_token":"tok_123","private_key_path":"/tmp/key.pem","nested":{"password":"pass"},"safe":"display"});
        let redacted = redact_copy(secret);
        assert_eq!(redacted["github_token"], REDACTED);
        assert_eq!(redacted["nested"]["password"], REDACTED);
        assert_eq!(redacted["safe"], "display");
    }

    #[test]
    fn redacted_fields_restore_without_changing_credentials() {
        let original = json!({"token":"original","setting":1});
        let mut edited = redact_copy(original.clone());
        edited["setting"] = json!(2);
        restore_redacted(&mut edited, &original);
        assert_eq!(edited["token"], "original");
        assert_eq!(edited["setting"], 2);
    }
}
