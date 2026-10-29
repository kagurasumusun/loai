//! Interactive approval UI with colored diff previews.

use crate::agent::tools::{ActionView, Decision};
use crate::tui;
use std::io::{BufRead, Write};

/// Render a line-diff preview for an edit (old_text → new_text).
pub fn render_edit_diff(path: &str, old_text: &str, new_text: &str, occurrences: usize, replace_all: bool) -> String {
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();

    // common prefix/suffix for context
    let prefix = old_lines.iter().zip(new_lines.iter()).take_while(|(a, b)| a == b).count();
    let suffix = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
        .min(old_lines.len().saturating_sub(prefix))
        .min(new_lines.len().saturating_sub(prefix));

    let old_mid = &old_lines[prefix..old_lines.len() - suffix];
    let new_mid = &new_lines[prefix..new_lines.len() - suffix];

    let mut out = String::new();
    if replace_all {
        out.push_str(&format!("--- {} ({} 箇所をすべて置換)\n", path, occurrences));
    } else {
        out.push_str(&format!("--- {} (1 箇所を置換)\n", path));
    }
    out.push_str(&format!("@@ -{},{} +{},{} @@\n", prefix + 1, old_mid.len(), prefix + 1, new_mid.len()));
    for l in old_mid.iter().take(40) {
        out.push_str(&tui::red(&format!("- {}", l)));
        out.push('\n');
    }
    if old_mid.len() > 40 {
        out.push_str(&format!("- ... 他 {} 行\n", old_mid.len() - 40));
    }
    for l in new_mid.iter().take(60) {
        out.push_str(&tui::green(&format!("+ {}", l)));
        out.push('\n');
    }
    if new_mid.len() > 60 {
        out.push_str(&format!("+ ... 他 {} 行\n", new_mid.len() - 60));
    }
    out
}

/// Preview for a full-file rewrite.
pub fn render_replace_preview(old: &str, new: &str) -> String {
    let old_n = old.lines().count();
    let new_n = new.lines().count();
    format!(
        "{} 行 → {} 行\n\n{}",
        tui::red(&old_n.to_string()),
        tui::green(&new_n.to_string()),
        render_edit_diff("(内容)", &old, &new, 1, false)
    )
}

/// The interactive approver used by the agent.
pub fn interactive_approver(view: &ActionView) -> Decision {
    println!();
    println!("{}", tui::cyan(&format!("◆ 承認が必要: {} {}", view.kind, view.title)));
    let plain = strip_ansi(&view.preview);
    let mut count = 0;
    for line in plain.lines() {
        if count >= 80 {
            println!("  ... (以下省略)");
            break;
        }
        let styled = if line.starts_with('-') {
            tui::red(line)
        } else if line.starts_with('+') {
            tui::green(line)
        } else {
            line.to_string()
        };
        println!("  {}", styled);
        count += 1;
    }
    println!();
    loop {
        print!("{}", tui::yellow("[y]実行 [n]拒否 [a]この操作は以後自動許可 [q]エージェント停止 > "));
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return Decision::Abort;
        }
        match line.trim().to_lowercase().as_str() {
            "y" | "yes" => return Decision::Yes,
            "n" | "no" => return Decision::No,
            "a" => return Decision::Always,
            "q" | "quit" | "exit" => return Decision::Abort,
            "" => continue, // Enter defaults to... require explicit choice
            _ => {
                println!("{}", tui::dim("y / n / a / q のいずれかを入力してください"));
            }
        }
    }
}

fn strip_ansi(s: &str) -> String {
    // strip ESC[...m sequences so re-coloring per line works
    let re = regex::Regex::new("\u{1b}\\[[0-9;]*m").unwrap();
    re.replace_all(s, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diff_render() {
        let d = render_edit_diff("src/a.rs", "fn a() {\n    1\n}\n", "fn a() {\n    2\n}\n", 1, false);
        assert!(d.contains("-     1"));
        assert!(d.contains("+     2"));
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi(&tui::red("hi")), "hi");
    }
}
