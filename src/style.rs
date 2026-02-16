//! Terminal styling utilities for consistent CLI output.
//!
//! Respects `NO_COLOR` environment variable and terminal capabilities.

use std::{
   io::{self, Write},
   sync::OnceLock,
   thread,
   time::Duration,
};

use owo_colors::OwoColorize;

/// Whether color output is enabled (cached on first call).
static COLOR_ENABLED: OnceLock<bool> = OnceLock::new();

/// Whether stdout is piped (not a terminal), cached on first call.
static PIPE_MODE: OnceLock<bool> = OnceLock::new();

/// Check if colors should be used.
pub fn colors_enabled() -> bool {
   *COLOR_ENABLED.get_or_init(|| {
      // NO_COLOR takes precedence (https://no-color.org/)
      if std::env::var("NO_COLOR").is_ok() {
         return false;
      }
      // Check if stdout is a terminal and supports color
      supports_color::on(supports_color::Stream::Stdout).is_some_and(|level| level.has_basic)
   })
}

/// Check if stdout is piped (not a terminal).
///
/// When true, only the raw commit message should go to stdout;
/// all progress/status output should go to stderr.
pub fn pipe_mode() -> bool {
   *PIPE_MODE.get_or_init(|| {
      use std::io::IsTerminal;
      !std::io::stdout().is_terminal()
   })
}

// === Color Palette ===

/// Success: checkmarks, completed actions (green + bold).
pub fn success(s: &str) -> String {
   if colors_enabled() {
      s.green().bold().to_string()
   } else {
      s.to_string()
   }
}

/// Warning: soft limit violations, non-fatal issues (yellow).
pub fn warning(s: &str) -> String {
   if colors_enabled() {
      s.yellow().to_string()
   } else {
      s.to_string()
   }
}

/// Error: failures, hard errors (red + bold).
pub fn error(s: &str) -> String {
   if colors_enabled() {
      s.red().bold().to_string()
   } else {
      s.to_string()
   }
}

/// Info: informational messages (cyan).
pub fn info(s: &str) -> String {
   if colors_enabled() {
      s.cyan().to_string()
   } else {
      s.to_string()
   }
}

/// Print warning message, clearing any active spinner line first.
///
/// This ensures warnings appear on their own line even when a spinner is
/// active, by writing a carriage return + clear-line escape sequence before the
/// message.
pub fn warn(msg: &str) {
   if !pipe_mode() {
      // Clear current line in case spinner is active (stdout, not stderr)
      print!("\r\x1b[K");
      io::stdout().flush().ok();
   }
   eprintln!("{} {}", warning(icons::WARNING), warning(msg));
}

/// Dim: less important details, file paths (dimmed).
pub fn dim(s: &str) -> String {
   if colors_enabled() {
      s.dimmed().to_string()
   } else {
      s.to_string()
   }
}

/// Bold: headers, key values.
pub fn bold(s: &str) -> String {
   if colors_enabled() {
      s.bold().to_string()
   } else {
      s.to_string()
   }
}

/// Model name styling (magenta).
pub fn model(s: &str) -> String {
   if colors_enabled() {
      s.magenta().to_string()
   } else {
      s.to_string()
   }
}

/// Commit type styling (blue + bold).
pub fn commit_type(s: &str) -> String {
   if colors_enabled() {
      s.blue().bold().to_string()
   } else {
      s.to_string()
   }
}

/// Scope styling (cyan).
pub fn scope(s: &str) -> String {
   if colors_enabled() {
      s.cyan().to_string()
   } else {
      s.to_string()
   }
}

/// Get terminal width, capped at 120 columns.
pub fn term_width() -> usize {
   terminal_size::terminal_size()
      .map_or(80, |(w, _)| w.0 as usize)
      .min(120)
}

// === Unicode Box Drawing ===

/// Box drawing characters.
pub mod box_chars {
   pub const TOP_LEFT: char = '\u{256D}';
   pub const TOP_RIGHT: char = '\u{256E}';
   pub const BOTTOM_LEFT: char = '\u{2570}';
   pub const BOTTOM_RIGHT: char = '\u{256F}';
   pub const HORIZONTAL: char = '\u{2500}';
   pub const VERTICAL: char = '\u{2502}';
}

/// Wrap text to fit within a given width, preserving words.
fn wrap_line(line: &str, max_width: usize) -> Vec<String> {
   if line.is_empty() {
      return vec![String::new()];
   }

   let mut lines = Vec::new();
   let mut current = String::new();

   for word in line.split_whitespace() {
      let word_len = word.chars().count();
      let current_len = current.chars().count();

      if current.is_empty() {
         // First word on line - take it even if too long
         current = word.to_string();
      } else if current_len + 1 + word_len <= max_width {
         // Word fits with space
         current.push(' ');
         current.push_str(word);
      } else {
         // Word doesn't fit - start new line
         lines.push(current);
         current = word.to_string();
      }
   }

   if !current.is_empty() {
      lines.push(current);
   }

   lines
}

/// Render a box-framed message with word wrapping.
pub fn boxed_message(title: &str, content: &str, width: usize) -> String {
   use box_chars::*;

   let mut out = String::new();
   let inner_width = width.saturating_sub(4); // Account for "│ " and " │"

   // Top border with title
   let title_len = title.chars().count();
   let border_width = width.saturating_sub(2);
   let padding = border_width.saturating_sub(title_len + 2);
   let left_pad = padding / 2;
   let right_pad = padding - left_pad;

   out.push(TOP_LEFT);
   out.push_str(&HORIZONTAL.to_string().repeat(left_pad));
   out.push(' ');
   out.push_str(&if colors_enabled() {
      bold(title)
   } else {
      title.to_string()
   });
   out.push(' ');
   out.push_str(&HORIZONTAL.to_string().repeat(right_pad));
   out.push(TOP_RIGHT);
   out.push('\n');

   // Content lines with wrapping
   for line in content.lines() {
      let wrapped = wrap_line(line, inner_width);
      for wrapped_line in wrapped {
         out.push(VERTICAL);
         out.push(' ');
         let line_chars = wrapped_line.chars().count();
         out.push_str(&wrapped_line);
         let pad = inner_width.saturating_sub(line_chars);
         out.push_str(&" ".repeat(pad));
         out.push(' ');
         out.push(VERTICAL);
         out.push('\n');
      }
   }

   // Bottom border
   out.push(BOTTOM_LEFT);
   out.push_str(&HORIZONTAL.to_string().repeat(border_width));
   out.push(BOTTOM_RIGHT);

   out
}

/// Print an info message that clears any spinner line first.
pub fn print_info(msg: &str) {
   use std::io::IsTerminal;
   if std::io::stderr().is_terminal() && colors_enabled() {
      // Clear line, print message with newline
      eprintln!("\r\x1b[K{} {msg}", icons::INFO.cyan());
   } else {
      eprintln!("{} {msg}", icons::INFO);
   }
}

/// Horizontal separator line.
pub fn separator(width: usize) -> String {
   let line = box_chars::HORIZONTAL.to_string().repeat(width);
   if colors_enabled() { dim(&line) } else { line }
}

/// Section header with decorative lines.
pub fn section_header(title: &str, width: usize) -> String {
   let title_len = title.chars().count();
   let line_len = (width.saturating_sub(title_len + 2)) / 2;
   let line = box_chars::HORIZONTAL.to_string().repeat(line_len);

   if colors_enabled() {
      format!("{} {} {}", dim(&line), bold(title), dim(&line))
   } else {
      format!("{line} {title} {line}")
   }
}

// === Status Icons ===

pub mod icons {
   pub const SUCCESS: &str = "\u{2713}";
   pub const WARNING: &str = "\u{26A0}";
   pub const ERROR: &str = "\u{2717}";
   pub const INFO: &str = "\u{2139}";
   pub const ARROW: &str = "\u{2192}";
   pub const BULLET: &str = "\u{2022}";
   pub const CLIPBOARD: &str = "\u{1F4CB}";
   pub const SEARCH: &str = "\u{1F50D}";
   pub const ROBOT: &str = "\u{1F916}";
   pub const SAVE: &str = "\u{1F4BE}";
}

// === Spinner ===

const SPINNER_FRAMES: &[char] = &[
   '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
   '\u{2807}', '\u{280F}',
];

/// Run an async operation with a spinner animation. Falls back to static text
/// if not a TTY.
pub async fn with_spinner<F: Future<Output = T>, T>(message: &str, f: F) -> T {
   // No spinner if not a TTY or colors disabled
   if !colors_enabled() {
      eprintln!("{message}");
      return f.await;
   }
   let (tx, rx) = std::sync::mpsc::channel::<()>();
   let msg = message.to_string();

   let spinner = thread::spawn(move || {
      let mut idx = 0;
      loop {
         if rx.try_recv().is_ok() {
            // Clear spinner line and show success
            print!("\r\x1b[K{} {}\n", icons::SUCCESS.green(), msg);
            io::stdout().flush().ok();
            break;
         }
         print!("\r{} {}", SPINNER_FRAMES[idx].cyan(), msg);
         io::stdout().flush().ok();
         idx = (idx + 1) % SPINNER_FRAMES.len();
         thread::sleep(Duration::from_millis(80));
      }
   });

   let result = f.await;
   tx.send(()).ok();
   spinner.join().ok();
   result
}

/// Run an async operation with a spinner, but show error on failure.
pub async fn with_spinner_result<F: Future<Output = Result<T, E>>, T, E>(
   message: &str,
   f: F,
) -> Result<T, E> {
   if !colors_enabled() {
      eprintln!("{message}");
      return f.await;
   }
   let (tx, rx) = std::sync::mpsc::channel::<bool>();
   let msg = message.to_string();

   let spinner = thread::spawn(move || {
      let mut idx = 0;
      loop {
         match rx.try_recv() {
            Ok(success) => {
               let icon = if success {
                  icons::SUCCESS.green().to_string()
               } else {
                  icons::ERROR.red().to_string()
               };
               print!("\r\x1b[K{icon} {msg}\n");
               io::stdout().flush().ok();
               break;
            },
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {},
         }
         print!("\r{} {}", SPINNER_FRAMES[idx].cyan(), msg);
         io::stdout().flush().ok();
         idx = (idx + 1) % SPINNER_FRAMES.len();
         thread::sleep(Duration::from_millis(80));
      }
   });

   let result = f.await;
   tx.send(result.is_ok()).ok();
   spinner.join().ok();
   result
}
