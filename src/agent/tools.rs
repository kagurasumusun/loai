//! Agent tools: definitions (OpenAI JSON schema) and execution with
//! workspace sandboxing, output caps and approval gates for mutations.

use crate::tui;
use crate::util;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub const MAX_TOOL_OUTPUT_CHARS: usize = 8000;

/// What the approver sees before a mutation is applied.
pub struct ActionView<'a> {
    pub kind: &'a str, // "write_file" | "edit_file" | "run_command"
    pub title: String,
    pub preview: String,
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    Yes,
    No,
    Always,
    Abort,
}

pub type Approver<'a> = dyn FnMut(&ActionView) -> Decision + 'a;

pub enum ToolOutcome {
    /// text returned to the model
    Text(String),
    /// agent loop should stop; summary attached
    Finish(String),
    /// user requested abort
    Abort,
}

pub struct ToolEnv<'a> {
    pub root: &'a Path,
    pub auto_yes: bool,
    pub always_allowed: HashSet<String>,
    pub never_allowed: HashSet<String>,
}

impl<'a> ToolEnv<'a> {
    pub fn new(root: &'a Path, auto_yes: bool) -> Self {
        ToolEnv {
            root,
            auto_yes,
            always_allowed: HashSet::new(),
            never_allowed: HashSet::new(),
        }
    }
}

/// OpenAI function tool definitions (descriptions in English for small-model
/// reliability; UI text stays Japanese).
pub fn tool_defs() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "List files and directories in the workspace as a compact tree with sizes. Use this first to understand the project layout.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Optional subdirectory (relative path). Default: workspace root."},
                        "depth": {"type": "integer", "description": "Max directory depth. Default 3, max 6."}
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a text file with line numbers. Use offset/limit for large files. Always read before editing.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Relative path from the workspace root."},
                        "start_line": {"type": "integer", "description": "1-based first line to read. Default 1."},
                        "max_lines": {"type": "integer", "description": "Max lines to return. Default 300, max 800."}
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "search_files",
                "description": "Search file CONTENTS by regex or plain text across the workspace. Returns path:line: text matches.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search text or regex pattern."},
                        "glob": {"type": "string", "description": "Optional file filter glob, e.g. \"*.rs\" or \"src/**\"."},
                        "is_regex": {"type": "boolean", "description": "Treat query as regex. Default false (plain text)."},
                        "case_sensitive": {"type": "boolean", "description": "Default false."},
                        "max_results": {"type": "integer", "description": "Default 50."}
                    },
                    "required": ["query"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create a new file (or fully rewrite an existing one with approval). Parent directories are created automatically.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Replace an exact text snippet in an existing file. old_text must match the file EXACTLY (copy from read_file) and be unique unless replace_all=true. Prefer this over write_file for edits.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "old_text": {"type": "string", "description": "Exact text to replace (include surrounding context lines to make it unique)."},
                        "new_text": {"type": "string", "description": "Replacement text."},
                        "replace_all": {"type": "boolean", "description": "Replace every occurrence. Default false."}
                    },
                    "required": ["path", "old_text", "new_text"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "Run a shell command in the workspace (build, test, git status...). Output is captured and returned. Requires user approval. Timeout 120s default.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout_sec": {"type": "integer", "description": "Default 120, max 600."}
                    },
                    "required": ["command"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "finish",
                "description": "Call this when the task is complete or cannot proceed. Provide a concise summary of what was done (and any remaining issues).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "summary": {"type": "string"}
                    },
                    "required": ["summary"]
                }
            }
        }),
    ]
}

fn s_arg(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("引数 {key} がありません"))
}

fn u_arg(args: &Value, key: &str, default: u64) -> u64 {
    args.get(key).and_then(|v| v.as_u64()).unwrap_or(default)
}

fn b_arg(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Resolve a tool path argument inside the workspace sandbox.
fn safe_path(env: &ToolEnv, arg: &str) -> Result<PathBuf> {
    if arg.contains("..") {
        return Err(anyhow!("相対パスに '..' は使えません: {arg}"));
    }
    let p = Path::new(arg);
    if p.is_absolute() {
        return Err(anyhow!("絶対パスは禁止されています。ワークスペースからの相対パスを使ってください: {arg}"));
    }
    let joined = env.root.join(arg);
    let canon_root = env
        .root
        .canonicalize()
        .unwrap_or_else(|_| env.root.to_path_buf());
    // parent must exist for canonicalize; for write it may not
    let parent = joined
        .parent()
        .ok_or_else(|| anyhow!("無効なパス: {arg}"))?;
    let canon_parent = if parent.exists() {
        parent.canonicalize()?
    } else {
        // allow creating new nested dirs, but validate against root lexically
        let lex = parent.to_path_buf();
        if lex.starts_with(&canon_root) || lex.starts_with(env.root) {
            lex
        } else {
            return Err(anyhow!("ワークスペース外へのアクセスは禁止: {arg}"));
        }
    };
    if !(canon_parent.starts_with(&canon_root) || canon_parent.starts_with(env.root)) {
        return Err(anyhow!("ワークスペース外へのアクセスは禁止: {arg}"));
    }
    let full = canon_parent.join(joined.file_name().ok_or_else(|| anyhow!("ファイル名が不正: {arg}"))?);
    Ok(full)
}

const DANGEROUS: &[&str] = &[
    "rm -rf /", "rm -rf ~", "format ", "del /f /s /q", "rd /s /q", "mkfs.",
    "shutdown", "reboot", "reg delete hklm", "curl | sh", "wget | sh",
    "remove-item -recurse -force c:\\", "gulp", ":(){", "dd if=/dev/zero",
];

pub fn execute(env: &mut ToolEnv, name: &str, args: &Value, approve: &mut Approver) -> Result<ToolOutcome> {
    match name {
        "list_files" => list_files(env, args),
        "read_file" => read_file(env, args),
        "search_files" => search_files(env, args),
        "write_file" => write_file(env, args, approve),
        "edit_file" => edit_file(env, args, approve),
        "run_command" => run_command(env, args, approve),
        "finish" => {
            let summary = s_arg(args, "summary").unwrap_or_else(|_| "完了".into());
            Ok(ToolOutcome::Finish(summary))
        }
        other => Ok(ToolOutcome::Text(format!(
            "ERROR: unknown tool '{other}'. Available: list_files, read_file, search_files, write_file, edit_file, run_command, finish"
        ))),
    }
}

fn cap_output(s: String) -> String {
    util::truncate_chars(&s, MAX_TOOL_OUTPUT_CHARS, "ツール出力が長大なため省略")
}

fn list_files(env: &ToolEnv, args: &Value) -> Result<ToolOutcome> {
    let sub = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let depth = u_arg(args, "depth", 3).clamp(1, 6);
    let base = if sub.is_empty() {
        env.root.to_path_buf()
    } else {
        match safe_path(env, sub) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutcome::Text(format!("ERROR: {e:#}"))),
        }
    };
    let rules = util::IgnoreRules::load(env.root);
    let files = util::walk_files(&base, &rules, 300, depth as usize + 2);
    if files.is_empty() {
        return Ok(ToolOutcome::Text("(ファイルが見つかりません)".into()));
    }
    let mut out = String::new();
    for (p, size) in files.iter().take(300) {
        let rel = p.strip_prefix(env.root).unwrap_or(p);
        out.push_str(&format!("{} ({})\n", rel.to_string_lossy().replace('\\', "/"), util::human_bytes(*size)));
    }
    if files.len() > 300 {
        out.push_str(&format!("... 他 {} ファイル\n", files.len() - 300));
    }
    Ok(ToolOutcome::Text(cap_output(out)))
}

fn read_file(env: &ToolEnv, args: &Value) -> Result<ToolOutcome> {
    let path = s_arg(args, "path")?;
    let full = match safe_path(env, &path) {
        Ok(p) => p,
        Err(e) => return Ok(ToolOutcome::Text(format!("ERROR: {e:#}"))),
    };
    if !full.exists() {
        return Ok(ToolOutcome::Text(format!("ERROR: file not found: {path}\nUse list_files or search_files to locate the file.")));
    }
    if util::looks_binary(&full) {
        return Ok(ToolOutcome::Text(format!("ERROR: {path} はバイナリファイルのため読み込めません")));
    }
    let text = util::read_text_capped(&full, 4 * 1024 * 1024)?;
    let lines: Vec<&str> = text.split('\n').collect();
    let total = lines.len();
    let start = u_arg(args, "start_line", 1).max(1) as usize;
    let max = u_arg(args, "max_lines", 300).clamp(1, 800) as usize;
    let begin = (start - 1).min(total);
    let end = (begin + max).min(total);
    let mut out = String::new();
    for (i, line) in lines[begin..end].iter().enumerate() {
        out.push_str(&format!("{:>5}| {}\n", begin + i + 1, line.trim_end_matches('\r')));
    }
    if end < total {
        out.push_str(&format!("[... {}/{} 行目まで表示。続きは start_line={} を指定]", end, total, end + 1));
    }
    Ok(ToolOutcome::Text(cap_output(out)))
}

fn search_files(env: &ToolEnv, args: &Value) -> Result<ToolOutcome> {
    let query = s_arg(args, "query")?;
    let glob = args.get("glob").and_then(|v| v.as_str()).unwrap_or("");
    let is_regex = b_arg(args, "is_regex");
    let case_sensitive = b_arg(args, "case_sensitive");
    let max_results = u_arg(args, "max_results", 50).clamp(1, 200) as usize;

    let pattern = if is_regex {
        query.clone()
    } else {
        regex::escape(&query)
    };
    let re = regex::RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .size_limit(4 * 1024 * 1024)
        .build()
        .map_err(|e| anyhow!("正規表現が不正です: {e}"))?;

    let rules = util::IgnoreRules::load(env.root);
    let files = util::walk_files(env.root, &rules, 3000, 12);
    let mut results = String::new();
    let mut count = 0usize;
    let mut truncated = false;
    'outer: for (p, _) in &files {
        let rel = p.strip_prefix(env.root).unwrap_or(p);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if !glob.is_empty() && !util::glob_match(glob, &rel_str) {
            continue;
        }
        if util::looks_binary(p) {
            continue;
        }
        let Ok(text) = util::read_text_capped(p, 2 * 1024 * 1024) else { continue };
        for (i, line) in text.split('\n').enumerate() {
            if re.is_match(line) {
                let trimmed: String = line.trim().chars().take(200).collect();
                results.push_str(&format!("{}:{}: {}\n", rel_str, i + 1, trimmed));
                count += 1;
                if count >= max_results {
                    truncated = true;
                    break 'outer;
                }
            }
        }
    }
    if count == 0 {
        return Ok(ToolOutcome::Text(format!("マッチなし: {query}")));
    }
    if truncated {
        results.push_str(&format!("[... 上位 {count} 件のみ表示]"));
    }
    Ok(ToolOutcome::Text(cap_output(results)))
}

fn write_file(env: &mut ToolEnv, args: &Value, approve: &mut Approver) -> Result<ToolOutcome> {
    let path = s_arg(args, "path")?;
    let content = s_arg(args, "content")?;
    let full = match safe_path(env, &path) {
        Ok(p) => p,
        Err(e) => return Ok(ToolOutcome::Text(format!("ERROR: {e:#}"))),
    };
    let exists = full.exists();
    if exists && util::looks_binary(&full) {
        return Ok(ToolOutcome::Text(format!("ERROR: {path} はバイナリファイルです")));
    }

    let preview = if exists {
        let old = util::read_text_capped(&full, 1024 * 1024).unwrap_or_default();
        format!("既存ファイルを完全に上書きします:\n\n{} → {}\n", crate::agent::approve::render_replace_preview(&old, &content), path)
    } else {
        format!("新規ファイル {} ({}):\n{}\n", path, util::human_bytes(content.len() as u64), util::truncate_chars(&content, 4000, "本文省略"))
    };

    let decision = gate(env, "write_file", &path, &preview, approve)?;
    match decision {
        Decision::No => Ok(ToolOutcome::Text(format!("ユーザーが {path} への書き込みを拒否しました。別の方法を提案するか finish してください。"))),
        _ => {
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&full, content.as_bytes())?;
            Ok(ToolOutcome::Text(format!(
                "OK: wrote {} ({} bytes) to {path}",
                util::human_bytes(content.len() as u64),
                content.len()
            )))
        }
    }
}

fn edit_file(env: &mut ToolEnv, args: &Value, approve: &mut Approver) -> Result<ToolOutcome> {
    let path = s_arg(args, "path")?;
    let old_text = s_arg(args, "old_text")?;
    let new_text = s_arg(args, "new_text")?;
    let replace_all = b_arg(args, "replace_all");
    let full = match safe_path(env, &path) {
        Ok(p) => p,
        Err(e) => return Ok(ToolOutcome::Text(format!("ERROR: {e:#}"))),
    };
    if !full.exists() {
        return Ok(ToolOutcome::Text(format!("ERROR: file not found: {path}. read_file で確認してください。")));
    }
    if util::looks_binary(&full) {
        return Ok(ToolOutcome::Text(format!("ERROR: {path} はバイナリファイルです")));
    }
    let content = util::read_text_capped(&full, 8 * 1024 * 1024)?;
    let occurrences = content.matches(&old_text).count();
    if occurrences == 0 {
        let hint = nearest_hint(&content, &old_text);
        return Ok(ToolOutcome::Text(format!(
            "ERROR: old_text が {path} 内で見つかりません (0 件)。read_file で正確な内容 (行末・インデント含む) をコピーしてください。{hint}"
        )));
    }
    if occurrences > 1 && !replace_all {
        let mut locs = String::new();
        for (i, (byte, _)) in content.match_indices(&old_text).take(5).enumerate() {
            let line = content[..byte].matches('\n').count() + 1;
            locs.push_str(&format!(" {}", line));
            let _ = i;
        }
        return Ok(ToolOutcome::Text(format!(
            "ERROR: old_text が {occurrences} 件見つかりました (行:{locs} ...)。一意になるよう前後の行を追加するか replace_all=true を使ってください。"
        )));
    }

    let new_content = if replace_all {
        content.replace(&old_text, &new_text)
    } else {
        content.replacen(&old_text, &new_text, 1)
    };

    let diff = crate::agent::approve::render_edit_diff(&path, &old_text, &new_text, occurrences, replace_all);
    let decision = gate(env, "edit_file", &path, &diff, approve)?;
    match decision {
        Decision::No => Ok(ToolOutcome::Text(format!("ユーザーが {path} の編集を拒否しました。"))),
        _ => {
            fs::write(&full, new_content.as_bytes())?;
            Ok(ToolOutcome::Text(format!(
                "OK: edited {path} ({} 箇所を置換)",
                if replace_all { occurrences } else { 1 }
            )))
        }
    }
}

/// Give the model a hint about near-misses (whitespace issues).
fn nearest_hint(content: &str, old_text: &str) -> String {
    let first_line = old_text.lines().next().unwrap_or("");
    if first_line.trim().is_empty() {
        return String::new();
    }
    let re = regex::Regex::new(&format!(r"(?i)^\s*{}$", regex::escape(first_line.trim()))).unwrap();
    for (i, line) in content.lines().enumerate() {
        if re.is_match(line) {
            return format!("\nHINT: 類似行が {} 行目にあります (インデント/空白の違いの可能性): '{}'", i + 1, line.trim());
        }
    }
    String::new()
}

fn gate(env: &mut ToolEnv, kind: &str, target: &str, preview: &str, approve: &mut Approver) -> Result<Decision> {
    if env.auto_yes {
        return Ok(Decision::Yes);
    }
    if env.never_allowed.contains(kind) {
        return Ok(Decision::No);
    }
    if env.always_allowed.contains(kind) {
        return Ok(Decision::Yes);
    }
    let view = ActionView {
        kind,
        title: target.to_string(),
        preview: preview.to_string(),
    };
    let d = approve(&view);
    match d {
        Decision::Always => {
            env.always_allowed.insert(kind.to_string());
            Ok(Decision::Yes)
        }
        Decision::Abort => Ok(Decision::Abort),
        other => Ok(other),
    }
}

fn run_command(env: &mut ToolEnv, args: &Value, approve: &mut Approver) -> Result<ToolOutcome> {
    let command = s_arg(args, "command")?;
    let timeout = u_arg(args, "timeout_sec", 120).clamp(5, 600) as u64;

    let lower = command.to_lowercase();
    if DANGEROUS.iter().any(|d| lower.contains(d)) {
        return Ok(ToolOutcome::Text(format!(
            "ERROR: 危険なコマンドとして拒否されました: {command}\n破壊的操作は手動で実行してください。"
        )));
    }

    let decision = gate(
        env,
        "run_command",
        &command,
        &format!("ワークスペースで実行:\n  {}\n(タイムアウト {timeout}秒)", tui::yellow(&command)),
        approve,
    )?;
    match decision {
        Decision::No => Ok(ToolOutcome::Text("ユーザーがコマンド実行を拒否しました。".into())),
        Decision::Abort => Ok(ToolOutcome::Abort),
        Decision::Yes | Decision::Always => {
            let (tx_out, rx_out) = mpsc::channel::<Vec<u8>>();
            let mut cmd = if cfg!(windows) {
                let mut c = Command::new("cmd");
                c.args(["/S", "/C", &command]);
                c
            } else {
                let mut c = Command::new("sh");
                c.args(["-c", &command]);
                c
            };
            cmd.current_dir(env.root)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000);
            }
            let mut child = cmd.spawn().map_err(|e| anyhow!("コマンド起動失敗: {e}"))?;

            // drain stdout+stderr in threads to avoid pipe deadlock
            if let Some(mut so) = child.stdout.take() {
                let tx = tx_out.clone();
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let _ = so.read_to_end(&mut buf);
                    let _ = tx.send(buf);
                });
            }
            if let Some(mut se) = child.stderr.take() {
                let tx = tx_out.clone();
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let _ = se.read_to_end(&mut buf);
                    let _ = tx.send(buf);
                });
            }
            drop(tx_out);

            let deadline = std::time::Instant::now() + Duration::from_secs(timeout);
            loop {
                match child.try_wait()? {
                    Some(status) => {
                        let mut chunks: Vec<Vec<u8>> = Vec::new();
                        while let Ok(c) = rx_out.try_recv() {
                            chunks.push(c);
                        }
                        let mut combined: Vec<u8> = chunks.concat();
                        // order is racy but acceptable for a coding agent
                        let mut text = String::from_utf8_lossy(&combined).into_owned();
                        let code = status.code().unwrap_or(-1);
                        if text.trim().is_empty() {
                            text = "(出力なし)".to_string();
                        }
                        combined.clear();
                        return Ok(ToolOutcome::Text(cap_output(format!("exit code: {code}\n{text}"))));
                    }
                    None => {
                        if std::time::Instant::now() > deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Ok(ToolOutcome::Text(format!("ERROR: タイムアウト ({timeout}s) — コマンドを強制終了しました")));
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_ws() -> PathBuf {
        let d = std::env::temp_dir().join(format!("loai-test-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn test_read_and_edit_flow() {
        let ws = tmp_ws();
        let f = ws.join("a.txt");
        let mut fh = fs::File::create(&f).unwrap();
        fh.write_all(b"fn main() {\n    todo!()\n}\n").unwrap();
        drop(fh);

        let mut env = ToolEnv::new(&ws, false);
        env.auto_yes = true;
        let mut approve = |_v: &ActionView| Decision::Yes;

        // read
        let out = execute(&mut env, "read_file", &json!({"path": "a.txt"}), &mut approve).unwrap();
        match out {
            ToolOutcome::Text(t) => assert!(t.contains("todo!()")),
            _ => panic!("expected text"),
        }

        // edit (auto_yes)
        let out = execute(
            &mut env,
            "edit_file",
            &json!({"path": "a.txt", "old_text": "todo!()", "new_text": "println!(\"hi\")"}),
            &mut approve,
        )
        .unwrap();
        match out {
            ToolOutcome::Text(t) => assert!(t.starts_with("OK")),
            _ => panic!("expected text"),
        }
        assert_eq!(fs::read_to_string(&f).unwrap(), "fn main() {\n    println!(\"hi\")\n}\n");

        // unique-ness error
        let f2 = ws.join("b.txt");
        fs::write(&f2, "x\nx\n").unwrap();
        let out = execute(
            &mut env,
            "edit_file",
            &json!({"path": "b.txt", "old_text": "x", "new_text": "y"}),
            &mut approve,
        )
        .unwrap();
        match out {
            ToolOutcome::Text(t) => assert!(t.contains("ERROR")),
            _ => panic!("expected text"),
        }

        fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn test_sandbox_rejects_escape() {
        let ws = tmp_ws();
        let mut env = ToolEnv::new(&ws, false);
        env.auto_yes = true;
        let mut approve = |_v: &ActionView| Decision::Yes;
        let out = execute(&mut env, "read_file", &json!({"path": "../../etc/passwd"}), &mut approve).unwrap();
        match out {
            ToolOutcome::Text(t) => assert!(t.contains("ERROR") || t.starts_with("ERROR")),
            _ => panic!("expected error text"),
        }
        let out = execute(&mut env, "read_file", &json!({"path": "/etc/passwd"}), &mut approve).unwrap();
        match out {
            ToolOutcome::Text(t) => assert!(t.starts_with("ERROR")),
            _ => panic!("expected error text"),
        }
        fs::remove_dir_all(&ws).ok();
    }
}
