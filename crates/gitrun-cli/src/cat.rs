use crossterm::{
    cursor::{Hide, MoveTo, Show},
    execute, queue,
    style::{Attribute, Print, SetAttribute},
    terminal::{
        self, BeginSynchronizedUpdate, Clear, ClearType, DisableLineWrap, EnableLineWrap,
        EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use serde::Deserialize;

pub(crate) mod presenter;
use std::io::{self, IsTerminal, Stdout, Write};

const ANIMATION: &str = include_str!("cat/gitrun.json");

#[derive(Debug, Deserialize)]
struct CatAnimation {
    width: usize,
    height: usize,
    frames: Vec<CatFrame>,
}

#[derive(Debug, Deserialize)]
struct CatFrame {
    delay: u64,
    lines: Vec<String>,
}

struct TerminalSession {
    stdout: Stdout,
}

impl TerminalSession {
    fn start() -> io::Result<Self> {
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            DisableLineWrap,
            Hide,
            Clear(ClearType::All)
        )?;
        Ok(Self { stdout })
    }

    fn draw(
        &mut self,
        lines: &[String],
        terminal_width: u16,
        terminal_height: u16,
    ) -> io::Result<()> {
        let visible_width = terminal_width as usize;
        let frame_width = lines
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0)
            .min(visible_width);

        let left = terminal_width.saturating_sub(frame_width as u16) / 2;
        let top =
            terminal_height.saturating_sub(lines.len().min(terminal_height as usize) as u16) / 2;

        queue!(self.stdout, BeginSynchronizedUpdate, Clear(ClearType::All))?;
        for (row, line) in lines.iter().take(terminal_height as usize).enumerate() {
            let clipped: String = line
                .chars()
                .take(visible_width.saturating_sub(left as usize))
                .collect();
            queue!(
                self.stdout,
                MoveTo(left, top.saturating_add(row as u16)),
                Print(clipped)
            )?;
        }
        queue!(self.stdout, EndSynchronizedUpdate)?;
        self.stdout.flush()
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(
            self.stdout,
            SetAttribute(Attribute::Reset),
            Show,
            EnableLineWrap,
            LeaveAlternateScreen
        );
        let _ = self.stdout.flush();
    }
}

pub fn run() -> i32 {
    let stdout = io::stdout();
    if !stdout.is_terminal() {
        eprintln!("GitRun cat: interactive terminal required.");
        return 0;
    }

    let animation = match serde_json::from_str::<CatAnimation>(ANIMATION) {
        Ok(animation) if !animation.frames.is_empty() => animation,
        Ok(_) => {
            eprintln!("GitRun cat: animation contains no frames.");
            return 1;
        }
        Err(error) => {
            eprintln!("GitRun cat: invalid animation data: {error}");
            return 1;
        }
    };

    if animation.width == 0 || animation.height == 0 {
        eprintln!("GitRun cat: animation dimensions are invalid.");
        return 1;
    }

    let mut terminal = match TerminalSession::start() {
        Ok(terminal) => terminal,
        Err(error) => {
            eprintln!("GitRun cat: unable to enter terminal animation mode: {error}");
            return 1;
        }
    };

    for frame in &animation.frames {
        let (columns, rows) = terminal::size().unwrap_or((
            animation.width.min(u16::MAX as usize) as u16,
            animation.height.min(u16::MAX as usize) as u16,
        ));

        let cleaned = remove_phase_messages(&frame.lines);
        let fitted = fit_page(&cleaned, animation.width, columns as usize, rows as usize);

        if let Err(error) = terminal.draw(&fitted, columns, rows) {
            eprintln!("GitRun cat: terminal rendering failed: {error}");
            return 1;
        }

        let delay = if contains_readable_message(&cleaned) {
            frame.delay.max(300)
        } else {
            frame.delay
        };
        std::thread::sleep(std::time::Duration::from_millis(delay));
    }

    0
}

fn fit_page(
    lines: &[String],
    width: usize,
    terminal_columns: usize,
    terminal_rows: usize,
) -> Vec<String> {
    let usable_width = width.min(terminal_columns);
    let full = normalize_lines(lines, usable_width);

    if full.len() <= terminal_rows {
        return full;
    }

    let compact = compact_half_block(&full, usable_width);
    if compact.len() <= terminal_rows {
        return compact;
    }

    compact.into_iter().take(terminal_rows).collect()
}

fn normalize_lines(lines: &[String], width: usize) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            let mut normalized: String = line.chars().take(width).collect();
            let current_width = normalized.chars().count();
            if current_width < width {
                normalized.push_str(&" ".repeat(width - current_width));
            }
            normalized
        })
        .collect()
}

fn remove_phase_messages(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| {
            let trimmed = line.trim();
            !(trimmed.starts_with('[') && trimmed.contains("/8]"))
        })
        .cloned()
        .collect()
}

fn contains_readable_message(lines: &[String]) -> bool {
    lines.iter().any(|line| {
        let trimmed = line.trim();
        trimmed.contains("~purr~")
            || trimmed.contains("<3")
            || trimmed.contains("Thanks for using GitRun!")
            || trimmed.contains("-Vider06")
    })
}

fn compact_half_block(lines: &[String], width: usize) -> Vec<String> {
    let mut compact_source = Vec::with_capacity(lines.len());
    let mut previous_was_blank = false;

    for line in lines {
        if line.trim().is_empty() {
            if previous_was_blank {
                continue;
            }
            previous_was_blank = true;
            continue;
        }
        previous_was_blank = false;
        compact_source.push(line.as_str());
    }

    let mut output = Vec::with_capacity(compact_source.len().div_ceil(2));
    let mut index = 0;

    while index < compact_source.len() {
        let top = compact_source[index];
        if preserve_text_line(top) {
            output.push(top.to_owned());
            index += 1;
            continue;
        }

        let bottom = compact_source
            .get(index + 1)
            .copied()
            .filter(|line| !preserve_text_line(line));

        output.push(half_block_row(top, bottom, width));
        index += if bottom.is_some() { 2 } else { 1 };
    }

    output
}

fn preserve_text_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("$ gitrun ")
        || trimmed.contains("~purr~")
        || trimmed.contains("<3")
        || trimmed.contains("Thanks for using GitRun!")
        || trimmed.contains("I have for this project spread to everyone")
        || trimmed.contains("-Vider06")
}

fn half_block_row(top: &str, bottom: Option<&str>, width: usize) -> String {
    let top_chars: Vec<char> = top.chars().collect();
    let bottom_chars: Vec<char> = bottom.unwrap_or("").chars().collect();
    let mut output = String::with_capacity(width);

    for column in 0..width {
        let top_set = top_chars.get(column).is_some_and(|ch| !ch.is_whitespace());
        let bottom_set = bottom_chars
            .get(column)
            .is_some_and(|ch| !ch.is_whitespace());

        output.push(match (top_set, bottom_set) {
            (false, false) => ' ',
            (true, false) => top_chars[column],
            (false, true) => bottom_chars[column],
            (true, true) => {
                let top = top_chars[column];
                let bottom = bottom_chars[column];
                if top == bottom {
                    top
                } else {
                    '#'
                }
            }
        });
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lines_pads_and_truncates() {
        let lines = vec!["abc".to_owned(), "abcdef".to_owned()];
        assert_eq!(
            normalize_lines(&lines, 4),
            vec!["abc ".to_owned(), "abcd".to_owned()]
        );
    }

    #[test]
    fn half_block_preserves_vertical_information() {
        assert_eq!(half_block_row("# ", Some(" #"), 2), "##");
        assert_eq!(half_block_row("##", Some("##"), 2), "##");
        assert_eq!(half_block_row("#:", Some(": "), 2), "#:");
    }

    #[test]
    fn phase_messages_are_removed() {
        let lines = vec![
            "$ gitrun --docker toolbox".to_owned(),
            "[1/8] il gatto arriva...".to_owned(),
            "  ~purr~".to_owned(),
        ];
        let cleaned = remove_phase_messages(&lines);
        assert_eq!(cleaned.len(), 2);
        assert!(!cleaned.iter().any(|line| line.contains("[1/8]")));
        assert!(cleaned.iter().any(|line| line.contains("~purr~")));
    }

    #[test]
    fn readable_messages_get_more_time() {
        let lines = vec!["  ~purr~".to_owned()];
        assert!(contains_readable_message(&lines));
    }

    #[test]
    fn final_message_is_detected_after_line_wrap() {
        let lines = vec![
            "Thanks for using GitRun! May the same love".to_owned(),
            "I have for this project spread to everyone".to_owned(),
        ];
        assert!(contains_readable_message(&lines));
    }

    #[test]
    fn final_message_continuation_is_preserved_during_compaction() {
        let line = "|                I have for this project spread to everyone                |";
        assert!(preserve_text_line(line));
    }

    #[test]
    fn actual_animation_fits_typical_terminal() {
        let animation = serde_json::from_str::<CatAnimation>(ANIMATION).unwrap();
        for frame in &animation.frames {
            let cleaned = remove_phase_messages(&frame.lines);
            let fitted = fit_page(&cleaned, animation.width, 120, 24);
            assert!(fitted.len() <= 24);
        }
    }

    #[test]
    fn fit_page_uses_full_when_it_fits() {
        let lines = vec!["##".to_owned(), "##".to_owned(), "  ~purr~".to_owned()];
        let normalized = normalize_lines(&lines, 12);
        assert_eq!(fit_page(&lines, 12, 80, 3), normalized);
        assert!(fit_page(&lines, 12, 80, 2).len() <= 2);
    }
}
