//! Context management: repository map generation, token budgeting,
//! deterministic trimming of old tool outputs (the 16GB RAM survival kit).

use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

use crate::util;

/// Build a compact repo map (tree with sizes) capped to ~`max_chars`.
pub fn build_repo_map(root: &Path, max_chars: usize) -> String {
    let rules = util::IgnoreRules::load(root);
    let files = util::walk_files(root, &rules, 2000, 10);
    if files.is_empty() {
        return "(ワークスペースにテキストファイルがありません)".to_string();
    }

    let mut lines: Vec<String> = Vec::new();
    let mut dirs = std::collections::BTreeMap::new();
    let mut root_files: Vec<String> = Vec::new();

    for (p, size) in &files {
        let rel = p.strip_prefix(root).unwrap_or(p);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        match rel.parent() {
            Some(dir) => {
                let dir_str = dir.to_string_lossy().replace('\\', "/");
                dirs.entry(dir_str)
                    .or_insert_with(|| Vec::new())
                    .push(format!("{} ({})", rel_str.rsplit('/').next().unwrap_or("?"), util::human_bytes(*size)));
            }
            None => root_files.push(format!("{} ({})", rel_str, util::human_bytes(*size))),
        }
    }

    // group by top-level dir for compactness
    let mut top: std::collections::BTreeMap<String, (usize, u64)> = std::collections::BTreeMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    for dir in dirs.keys() {
        let top_dir = dir.split('/').next().unwrap_or(dir).to_string();
        seen.insert(dir.clone());
        let e = top.entry(top_dir).or_insert((0, 0));
        e.0 += 1;
    }
    for (p, size) in &files {
        let rel = p.strip_prefix(root).unwrap_or(p);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if !rel_str.contains('/') {
            let e = top.entry("(ルート)".to_string()).or_insert((0, 0));
            e.0 += 1;
            e.1 += *size;
        } else {
            let t = rel_str.split('/').next().unwrap_or("?").to_string();
            let e = top.entry(t).or_insert((0, 0));
            e.1 += *size;
        }
    }

    for (dir, (count, size)) in &top {
        if dir == "(ルート)" {
            continue;
        }
        lines.push(format!("{}/ ({} ファイル, {})", dir, count, util::human_bytes(*size)));
    }
    for f in &root_files {
        lines.push(f.clone());
    }
    // detail for small dirs
    let mut detail = String::new();
    if files.len() <= 120 {
        for (dir, fs) in &dirs {
            if fs.len() <= 30 {
                detail.push_str(&format!("  {}\n    {}\n", dir, fs.join(", ")));
            } else {
                detail.push_str(&format!("  {} ({} ファイル)\n", dir, fs.len()));
            }
        }
    }

    let mut map = lines.join("\n");
    if !detail.is_empty() {
        map.push_str("\n\n詳細:\n");
        map.push_str(&detail);
    }
    util::truncate_chars(&map, max_chars, "リポジトリマップ省略")
}

/// Estimate tokens for a whole message list.
pub fn estimate_messages(messages: &[Value]) -> usize {
    let mut total = 0;
    for m in messages {
        total += 8; // role + framing overhead
        if let Some(c) = m.get("content").and_then(|c| c.as_str()) {
            total += util::estimate_tokens(c);
        }
        if let Some(tcs) = m.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                total += util::estimate_tokens(&tc.to_string());
            }
        }
    }
    total
}

/// Deterministically trim messages to fit `budget` tokens.
/// Strategy: keep system message, the first user task, and the most recent
/// messages. Elide old tool RESULTS first (they are the memory hogs), then
/// middle user/assistant contents.
pub fn trim(messages: &mut Vec<Value>, budget: usize) {
    if estimate_messages(messages) <= budget || messages.len() < 4 {
        return;
    }
    let protected_head = count_head(messages);
    let last_keep = 4.min(messages.len()); // always keep tail

    // pass 1: elide old tool outputs
    for i in protected_head..messages.len().saturating_sub(last_keep) {
        if estimate_messages(messages) <= budget {
            return;
        }
        let is_tool = messages[i].get("role").and_then(|r| r.as_str()) == Some("tool");
        if is_tool {
            if let Some(c) = messages[i].get("content").and_then(|c| c.as_str()) {
                let note = format!("[古いツール出力を省略 ({}文字)]", c.chars().count());
                messages[i]["content"] = Value::String(note);
            }
        }
    }

    // pass 2: elide middle assistant/user contents
    for i in protected_head..messages.len().saturating_sub(last_keep) {
        if estimate_messages(messages) <= budget {
            return;
        }
        let role = messages[i].get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "assistant" {
            let has_tc = messages[i].get("tool_calls").map(|t| !t.is_null()).unwrap_or(false);
            if !has_tc {
                messages[i]["content"] = Value::String("[省略]".to_string());
            }
        } else if role == "user" {
            if let Some(c) = messages[i].get("content").and_then(|c| c.as_str()) {
                let note = format!("[古い指示を省略 ({}文字)]", c.chars().count());
                messages[i]["content"] = Value::String(note);
            }
        }
    }

    // pass 3: drop fully-elided middle messages entirely (keep pairing valid:
    // a tool msg must follow its assistant tool_calls msg, so only drop user/assistant)
    let mut i = protected_head;
    while i < messages.len().saturating_sub(last_keep) {
        if estimate_messages(messages) <= budget {
            break;
        }
        let role = messages[i].get("role").and_then(|r| r.as_str()).unwrap_or("");
        if (role == "user" || role == "assistant") && i + 1 < messages.len() {
            let next_role = messages[i + 1].get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role == "assistant" && next_role != "tool" {
                messages.remove(i);
                continue;
            }
            if role == "user" {
                messages.remove(i);
                continue;
            }
        }
        i += 1;
    }
}

/// How many messages at the head must never be touched (system + first user task).
fn count_head(messages: &[Value]) -> usize {
    let mut n = 0;
    for m in messages {
        match m.get("role").and_then(|r| r.as_str()) {
            Some("system") => n += 1,
            Some("user") => return n + 1,
            _ => break,
        }
    }
    n.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: &str, content: &str) -> Value {
        json!({"role": role, "content": content})
    }

    #[test]
    fn test_trim_elides_old_tool_outputs() {
        let mut messages = vec![msg("system", &"x".repeat(200)), msg("user", &"タスク".repeat(10))];
        for i in 0..12 {
            messages.push(json!({"role": "assistant", "content": "ok", "tool_calls": [{"id": format!("c{i}"), "type": "function", "function": {"name": "read_file", "arguments": "{}"}}]}));
            messages.push(json!({"role": "tool", "tool_call_id": format!("c{i}"), "content": "A".repeat(2000)}));
        }
        messages.push(msg("user", "続けて"));
        let before = estimate_messages(&messages);
        trim(&mut messages, 3000);
        let after = estimate_messages(&messages);
        assert!(after <= 3000, "budget exceeded: {after}");
        assert!(after < before);
        // system + first task kept
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn test_estimate() {
        let m = vec![msg("system", "hello"), msg("user", "こんにちは")];
        assert!(estimate_messages(&m) > 16);
    }
}
