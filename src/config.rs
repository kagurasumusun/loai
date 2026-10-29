//! loai configuration and data directories (~/.loai).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// Default model profile id (e.g. "qwen3-8b")
    pub model: Option<String>,
    /// Backend: "auto" | "vulkan" | "cpu" | "sycl"
    pub backend: Option<String>,
    /// Context size override
    pub ctx: Option<u32>,
    /// Threads override
    pub threads: Option<u32>,
    /// GPU layers override (0..N, "auto" resolved at launch)
    pub ngl: Option<u32>,
    /// KV cache quant override ("q8_0" etc.)
    pub kv_quant: Option<String>,
    /// External OpenAI-compatible endpoint (skips local engine entirely)
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    /// Agent max turns
    pub max_turns: Option<u32>,
    /// Auto-approve all tool actions (dangerous)
    pub yes: bool,
}

pub fn loai_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".loai")
}

pub fn models_dir() -> PathBuf {
    loai_dir().join("models")
}

pub fn engines_dir() -> PathBuf {
    loai_dir().join("engines")
}

pub fn logs_dir() -> PathBuf {
    loai_dir().join("logs")
}

pub fn tmp_dir() -> PathBuf {
    loai_dir().join("tmp")
}

pub fn pid_file() -> PathBuf {
    loai_dir().join("llama-server.pid")
}

pub fn ensure_dirs() -> Result<()> {
    for d in [loai_dir(), models_dir(), engines_dir(), logs_dir(), tmp_dir()] {
        fs::create_dir_all(&d).with_context(|| format!("ディレクトリを作成できません: {}", d.display()))?;
    }
    Ok(())
}

pub fn config_path() -> PathBuf {
    loai_dir().join("config.json")
}

pub fn load_config() -> Config {
    match fs::read_to_string(config_path()) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

pub fn save_config(cfg: &Config) -> Result<()> {
    ensure_dirs()?;
    let text = serde_json::to_string_pretty(cfg)?;
    fs::write(config_path(), text).with_context(|| "config.json の保存に失敗")?;
    Ok(())
}
