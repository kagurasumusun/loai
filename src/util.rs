//! Shared utilities: paths, sizes, token estimation, glob, workspace walking.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const BINARY_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "ico", "icns", "pdf", "zip", "gz", "xz", "zst", "tar", "7z",
    "exe", "dll", "so", "dylib", "bin", "o", "a", "class", "jar", "pyc", "woff", "woff2", "ttf",
    "otf", "eot", "mp3", "mp4", "avi", "mov", "webm", "wav", "flac", "gguf", "onnx", "pt", "pth",
    "safetensors", "ckpt", "img", "iso", "dmg", "msi", "deb", "rpm", "appimage", "loai-manifest",
];

pub const IGNORED_DIRS: &[&str] = &[
    ".git", "node_modules", "target", "dist", "build", "out", ".venv", "venv", "__pycache__",
    ".next", ".nuxt", ".cache", ".loai", ".idea", ".gradle", "coverage", ".mypy_cache",
    ".pytest_cache", ".tox", "vendor", "Pods", "DerivedData",
];

/// Human readable byte size (Japanese-friendly, e.g. 4.7GB).
pub fn human_bytes(n: u64) -> String {
    let f = n as f64;
    if n >= 1024 * 1024 * 1024 {
        format!("{:.1}GB", f / (1024.0 * 1024.0 * 1024.0))
    } else if n >= 1024 * 1024 {
        format!("{:.1}MB", f / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.1}KB", f / 1024.0)
    } else {
        format!("{}B", n)
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x9FFF   | // CJK radicals, Kangxi, hiragana/katakana, CJK unified
        0xF900..=0xFAFF   | // CJK compat ideographs
        0xFF00..=0xFFEF     // fullwidth forms
    )
}

/// Rough token estimate tuned for Qwen-family BPE:
/// ~1 token per CJK char, ~4 ascii chars per token.
pub fn estimate_tokens(s: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in s.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other / 4
}

/// Convert a simple glob pattern (*, **, ?) into an anchored regex string.
pub fn glob_to_regex(pat: &str) -> String {
    let mut re = String::with_capacity(pat.len() * 2);
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '*' {
            if i + 1 < chars.len() && chars[i + 1] == '*' {
                re.push_str(".*");
                i += 2;
                // "**/" also matches zero dirs
                if i < chars.len() && chars[i] == '/' {
                    re.push_str("(?:/|/.*/)");
                    i += 1;
                }
            } else {
                re.push_str("[^/]*");
                i += 1;
            }
        } else if c == '?' {
            re.push('.');
            i += 1;
        } else {
            re.push_str(&regex::escape(&c.to_string()));
            i += 1;
        }
    }
    format!("^{}$", re)
}

/// Simple glob matcher using the regex crate.
pub fn glob_match(pat: &str, text: &str) -> bool {
    match regex::Regex::new(&glob_to_regex(pat)) {
        Ok(re) => re.is_match(text),
        Err(_) => false,
    }
}

pub fn is_binary_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    BINARY_EXTS.iter().any(|e| lower.ends_with(e))
}

/// Loaded ignore rules: built-in dirs + user .gitignore patterns (relative).
pub struct IgnoreRules {
    pub dir_names: HashSet<String>,
    pub file_globs: Vec<String>,
}

impl IgnoreRules {
    pub fn load(root: &Path) -> Self {
        let mut dir_names: HashSet<String> = IGNORED_DIRS.iter().map(|s| s.to_string()).collect();
        let mut file_globs: Vec<String> = Vec::new();
        let gitignore = root.join(".gitignore");
        if let Ok(text) = fs::read_to_string(&gitignore) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                    continue;
                }
                let line = line.trim_start_matches('/');
                // directory pattern
                if line.ends_with('/') {
                    dir_names.insert(line.trim_end_matches('/').to_string());
                } else {
                    file_globs.push(line.to_string());
                }
            }
        }
        IgnoreRules { dir_names, file_globs }
    }

    pub fn skip_dir(&self, name: &str) -> bool {
        self.dir_names.contains(name)
    }

    pub fn skip_file(&self, rel: &str) -> bool {
        self.file_globs.iter().any(|g| glob_match(g, rel)) || self.file_globs.iter().any(|g| glob_match(g, &format!("**/{}", g)))
    }
}

/// Walk workspace files respecting ignore rules. Returns (path, size) pairs.
pub fn walk_files(root: &Path, rules: &IgnoreRules, max_entries: usize, max_depth: usize) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    walk_inner(root, root, rules, max_entries, max_depth, &mut out);
    out
}

fn walk_inner(
    root: &Path,
    dir: &Path,
    rules: &IgnoreRules,
    max_entries: usize,
    max_depth: usize,
    out: &mut Vec<(PathBuf, u64)>,
) {
    if out.len() >= max_entries || max_depth == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(|e| e.file_name());
    for entry in items {
        if out.len() >= max_entries {
            return;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if rules.skip_dir(&name) {
                continue;
            }
            walk_inner(root, &path, rules, max_entries, max_depth - 1, out);
        } else {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            if rules.skip_file(&rel) || is_binary_name(&name) {
                continue;
            }
            out.push((path, meta.len()));
        }
    }
}

/// Check whether a file looks binary (NUL byte in first 8KB).
pub fn looks_binary(path: &Path) -> bool {
    let Ok(mut f) = fs::File::open(path) else { return true };
    let mut buf = [0u8; 8192];
    match f.read(&mut buf) {
        Ok(n) => buf[..n].contains(&0),
        Err(_) => true,
    }
}

/// Read text with a byte cap (lossy conversion).
pub fn read_text_capped(path: &Path, cap: u64) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("ファイルを開けません: {}", path.display()))?;
    let mut bytes: Vec<u8> = Vec::new();
    let mut limited = (&mut f).take(cap);
    std::io::Read::read_to_end(&mut limited, &mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Truncate a string to at most `max_chars`, appending an ellipsis note.
pub fn truncate_chars(s: &str, max_chars: usize, note: &str) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{}\n\n[{}: 全 {} 文字中先頭 {} 文字のみ表示]", head, note, s.chars().count(), max_chars)
    }
}

/// Detect number of physical cores (best-effort, cross platform without deps).
pub fn physical_cores() -> usize {
    #[cfg(target_os = "windows")]
    {
        if let Some(n) = win_physical_cores() {
            return n;
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = fs::read_to_string("/proc/cpuinfo") {
            // count "core id" lines = physical cores across all sockets
            let n = text.lines().filter(|l| l.starts_with("core id")).count();
            if n > 0 {
                return n;
            }
        }
    }
    let logical = std::thread::available_parallelism().map(|v| v.get()).unwrap_or(4);
    logical
}

#[cfg(target_os = "windows")]
fn win_physical_cores() -> Option<usize> {
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "(Get-CimInstance Win32_Processor | Measure-Object -Property NumberOfCores -Sum).Sum"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    text.parse::<usize>().ok().map(|n| n.max(1))
}

/// Clamp threads for llama.cpp based on hybrid Intel CPUs (P+E cores).
pub fn llama_threads() -> usize {
    let phys = physical_cores();
    phys.clamp(2, 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokens() {
        assert!(estimate_tokens("hello world this is a test") > 0);
        assert!(estimate_tokens("こんにちは世界") >= 6);
    }

    #[test]
    fn test_glob() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(glob_match("src/**", "src/a/b.txt"));
        assert!(glob_match("a?c", "abc"));
    }

    #[test]
    fn test_human_bytes() {
        assert_eq!(human_bytes(500), "500B");
        assert_eq!(human_bytes(4683074336), "4.4GB");
    }
}
