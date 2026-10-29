//! Terminal UI helpers: colors (NO_COLOR aware), progress bar, status lines.

use crossterm::style::{Color, Stylize};
use std::io::IsTerminal;
use std::sync::OnceLock;
use std::time::Instant;

static COLOR_OK: OnceLock<bool> = OnceLock::new();

pub fn color_enabled() -> bool {
    *COLOR_OK.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some() {
            false
        } else {
            std::io::stdout().is_terminal()
        }
    })
}

pub fn paint(s: &str, c: Color) -> String {
    if color_enabled() {
        s.to_string().with(c).to_string()
    } else {
        s.to_string()
    }
}

pub fn info(msg: &str) {
    println!("{}", paint(&format!("ℹ {}", msg), Color::Cyan));
}

pub fn warn(msg: &str) {
    println!("{}", paint(&format!("⚠ {}", msg), Color::Yellow));
}

pub fn err(msg: &str) {
    println!("{}", paint(&format!("✖ {}", msg), Color::Red));
}

pub fn succ(msg: &str) {
    println!("{}", paint(&format!("✔ {}", msg), Color::Green));
}

pub fn dim(msg: &str) -> String {
    paint(msg, Color::DarkGrey)
}

pub fn bold(msg: &str) -> String {
    paint(msg, Color::White).to_string()
}

pub fn cyan(msg: &str) -> String {
    paint(msg, Color::Cyan)
}

pub fn green(msg: &str) -> String {
    paint(msg, Color::Green)
}

pub fn red(msg: &str) -> String {
    paint(msg, Color::Red)
}

pub fn yellow(msg: &str) -> String {
    paint(msg, Color::Yellow)
}

/// A lightweight single-line progress bar printed to stderr.
pub struct Progress {
    prefix: String,
    total: u64,
    start: Instant,
    last_print: Instant,
    is_tty: bool,
}

impl Progress {
    pub fn new(prefix: &str, total: u64) -> Self {
        Progress {
            prefix: prefix.to_string(),
            total,
            start: Instant::now(),
            last_print: Instant::now() - std::time::Duration::from_secs(1),
            is_tty: std::io::stderr().is_terminal(),
        }
    }

    pub fn update(&mut self, done: u64) {
        if !self.is_tty {
            // non-tty: print a line roughly every 5 seconds
            if self.last_print.elapsed().as_secs() >= 5 {
                self.last_print = Instant::now();
                eprintln!("[{}] {} / {}", self.prefix, crate::util::human_bytes(done), crate::util::human_bytes(self.total));
            }
            return;
        }
        if self.last_print.elapsed().as_millis() < 200 {
            return;
        }
        self.last_print = Instant::now();
        let pct = if self.total > 0 { done as f64 / self.total as f64 * 100.0 } else { 0.0 };
        let speed = done as f64 / self.start.elapsed().as_secs_f64().max(0.001);
        let width = 28usize;
        let filled = ((pct / 100.0) * width as f64) as usize;
        let bar: String = "█".repeat(filled.min(width)) + &"░".repeat(width.saturating_sub(filled));
        eprint!(
            "\r  {} |{}| {:>5.1}% {} {}/s   ",
            self.prefix,
            bar,
            pct,
            crate::util::human_bytes(speed as u64),
            crate::util::human_bytes(done)
        );
    }

    pub fn finish(&mut self, done: u64) {
        if self.is_tty {
            let secs = self.start.elapsed().as_secs_f64().max(0.001);
            let speed = done as f64 / secs;
            eprintln!(
                "\r  {} |{}] 100.0% {} {}/s ({:.1}s)",
                self.prefix,
                "█".repeat(28),
                crate::util::human_bytes(speed as u64),
                crate::util::human_bytes(done),
                secs
            );
        }
    }
}
