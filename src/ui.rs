use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;

static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// No spinners or progress output (MCP server mode: the terminal isn't ours).
pub fn set_quiet() {
    QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// A spinner that can be paused while the user is typing at a prompt.
pub struct Status {
    bar: Option<ProgressBar>,
}

impl Status {
    pub fn new(msg: &str) -> Self {
        let mut s = Self { bar: None };
        s.set(msg);
        s
    }

    pub fn set(&mut self, msg: &str) {
        if QUIET.load(std::sync::atomic::Ordering::Relaxed) || !console::Term::stderr().is_term() {
            return;
        }
        let bar = self.bar.get_or_insert_with(|| {
            let b = ProgressBar::new_spinner();
            // wide_msg truncates to the terminal width; a wrapped spinner line can't be redrawn.
            b.set_style(ProgressStyle::with_template("{spinner} {wide_msg}").unwrap());
            b.enable_steady_tick(Duration::from_millis(100));
            b
        });
        bar.set_message(msg.to_string());
    }

    pub fn println(&self, msg: &str) {
        match &self.bar {
            Some(b) => b.println(msg),
            None => eprintln!("{msg}"),
        }
    }

    pub fn pause(&mut self) {
        if let Some(b) = self.bar.take() {
            b.finish_and_clear();
        }
    }
}

impl Drop for Status {
    fn drop(&mut self) {
        self.pause();
    }
}

/// Runs `fut` behind a spinner (on stderr, only when interactive).
pub async fn with_spinner<T>(msg: &str, fut: impl std::future::Future<Output = T>) -> T {
    let mut s = Status::new(msg);
    let out = fut.await;
    s.pause();
    out
}

pub fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Usable terminal width for layout (stdout, then stderr; 100 when not a terminal).
pub fn width() -> usize {
    let size = console::Term::stdout()
        .size_checked()
        .or_else(|| console::Term::stderr().size_checked());
    size.map(|(_, w)| w as usize).unwrap_or(100).max(30)
}

/// Cuts `s` to `width` columns (ANSI-aware), ending in "…" when shortened.
pub fn fit(s: &str, width: usize) -> String {
    console::truncate_str(s, width, "…").into_owned()
}

/// Word-wraps `s` to `width` columns, indenting every line with `indent`.
pub fn wrap(s: &str, width: usize, indent: &str) -> Vec<String> {
    let opts = textwrap::Options::new(width)
        .initial_indent(indent)
        .subsequent_indent(indent);
    textwrap::wrap(s, opts)
        .into_iter()
        .map(|l| l.into_owned())
        .collect()
}

/// `left` and `right` on one line, `right` flush with column `width`; `left` is shortened if needed.
pub fn spread(left: &str, right: &str, width: usize) -> String {
    let rw = console::measure_text_width(right);
    let left = fit(left, width.saturating_sub(rw + 2));
    let gap = width
        .saturating_sub(console::measure_text_width(&left) + rw)
        .max(2);
    format!("{left}{}{right}", " ".repeat(gap))
}

/// Prints a hint on stderr, wrapped to the terminal.
pub fn hint(s: &str) {
    for line in wrap(s, width(), "") {
        eprintln!("{}", dim(&line));
    }
}

/// Labels for pickers: a label wider than the terminal wraps and breaks the picker's redraw.
pub fn picker_labels(labels: impl IntoIterator<Item = String>) -> Vec<String> {
    let w = width().saturating_sub(4);
    labels.into_iter().map(|l| fit(&l, w)).collect()
}

/// Formats euros the Dutch way: € 12,50.
pub fn euros(amount: f64) -> String {
    let cents = (amount * 100.0).round() as i64;
    let sign = if cents < 0 { "-" } else { "" };
    format!("{sign}€ {},{:02}", cents.abs() / 100, cents.abs() % 100)
}

pub fn euros_cents(cents: i64) -> String {
    euros(cents as f64 / 100.0)
}

pub fn dim(s: &str) -> String {
    console::style(s).dim().to_string()
}

pub fn bold(s: &str) -> String {
    console::style(s).bold().to_string()
}

pub fn interactive() -> bool {
    !QUIET.load(std::sync::atomic::Ordering::Relaxed)
        && console::Term::stdout().is_term()
        && console::Term::stderr().is_term()
}
