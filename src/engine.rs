//! llama.cpp engine management: locate official nightly binaries, install,
//! launch llama-server with per-profile tuning + automatic fallback ladder.

use crate::config;
use crate::models::Profile;
use crate::tui;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// llama.cpp nightly build that we fall back to if the GitHub API is unreachable.
pub const PINNED_LLAMA_BUILD: &str = "b11977";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Vulkan,
    Cpu,
    Sycl,
}

impl Backend {
    pub fn from_str(s: &str) -> Result<Backend> {
        match s.to_lowercase().as_str() {
            "vulkan" => Ok(Backend::Vulkan),
            "cpu" => Ok(Backend::Cpu),
            "sycl" => Ok(Backend::Sycl),
            "auto" => Ok(Backend::Vulkan),
            other => Err(anyhow!("不明なバックエンド: {other} (auto/vulkan/cpu/sycl)")),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Backend::Vulkan => "vulkan",
            Backend::Cpu => "cpu",
            Backend::Sycl => "sycl",
        }
    }

    fn asset_suffix(&self) -> &'static str {
        match self {
            Backend::Vulkan => {
                if cfg!(windows) { "bin-win-vulkan-x64.zip" } else { "bin-ubuntu-vulkan-x64.tar.gz" }
            }
            Backend::Cpu => {
                if cfg!(windows) { "bin-win-cpu-x64.zip" } else { "bin-ubuntu-x64.tar.gz" }
            }
            Backend::Sycl => {
                if cfg!(windows) { "bin-win-sycl-x64.zip" } else { "bin-ubuntu-sycl-fp16-x64.tar.gz" }
            }
        }
    }

    pub fn has_gpu(&self) -> bool {
        matches!(self, Backend::Vulkan | Backend::Sycl)
    }
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[allow(dead_code)]
    size: u64,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

pub struct AssetInfo {
    pub tag: String,
    pub name: String,
    pub url: String,
}

/// Find the newest llama.cpp release asset matching the backend.
pub fn find_asset(backend: Backend) -> Result<AssetInfo> {
    let suffix = backend.asset_suffix();
    let agent = crate::download::dl_agent(30);
    let mut req = agent.get("https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=20");
    if let Ok(tok) = std::env::var("GH_TOKEN") {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    if let Ok(resp) = req.call() {
        let text = resp.into_string().unwrap_or_default();
        if let Ok(rels) = serde_json::from_str::<Vec<GhRelease>>(&text) {
            for rel in &rels {
                for a in &rel.assets {
                    if a.name.ends_with(suffix) {
                        return Ok(AssetInfo {
                            tag: rel.tag_name.clone(),
                            name: a.name.clone(),
                            url: a.browser_download_url.clone(),
                        });
                    }
                }
            }
        }
    }
    // fallback: pinned build
    let name = format!("llama-{PINNED_LLAMA_BUILD}-{}", suffix);
    let url = format!("https://github.com/ggml-org/llama.cpp/releases/download/{PINNED_LLAMA_BUILD}/{name}");
    tui::warn(&format!("GitHub API から最新版を取得できなかったため、ピン留めビルド {PINNED_LLAMA_BUILD} を使用します"));
    Ok(AssetInfo { tag: PINNED_LLAMA_BUILD.to_string(), name, url })
}

pub fn server_exe_name() -> &'static str {
    if cfg!(windows) { "llama-server.exe" } else { "llama-server" }
}

/// Locate an installed llama-server exe for a backend (any build), newest first.
pub fn find_installed(backend: Backend) -> Option<(String, PathBuf)> {
    let root = config::engines_dir();
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(rd) = fs::read_dir(&root) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(&format!("-{}", backend.name())) && name.starts_with("llama-") {
                let tag = name
                    .strip_prefix("llama-")
                    .and_then(|r| r.rsplit_once('-'))
                    .map(|(t, _)| t.to_string())
                    .unwrap_or_default();
                if let Some(exe) = find_exe_recursive(&e.path()) {
                    candidates.push((tag, exe));
                }
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0)); // newest build number first
    candidates.into_iter().next()
}

fn find_exe_recursive(dir: &Path) -> Option<PathBuf> {
    let target = server_exe_name();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().map(|n| n == target).unwrap_or(false) {
                return Some(p);
            }
        }
    }
    None
}

/// Install (download + extract) the engine for a backend. Idempotent.
pub fn install_engine(backend: Backend) -> Result<(String, PathBuf)> {
    config::ensure_dirs()?;
    if let Some((tag, exe)) = find_installed(backend) {
        return Ok((tag, exe));
    }
    let asset = find_asset(backend)?;
    let dir = engine_dir(&asset.tag, backend);
    if dir.join(".loai-ok").exists() {
        if let Some(exe) = find_exe_recursive(&dir) {
            return Ok((asset.tag, exe));
        }
    }
    tui::info(&format!("llama.cpp ({} / {}) をダウンロード中...", asset.tag, backend.name()));
    let archive = config::tmp_dir().join(&asset.name);
    crate::download::download_unknown_size(&asset.url, &archive, &format!("llama-{}", backend.name()))?;

    fs::create_dir_all(&dir)?;
    tui::info("展開しています...");
    if asset.name.ends_with(".zip") {
        let f = fs::File::open(&archive)?;
        let mut z = zip::ZipArchive::new(f).context("zip を開けません")?;
        z.extract(&dir).context("zip の展開に失敗")?;
    } else {
        let out = Command::new("tar").arg("-xzf").arg(&archive).arg("-C").arg(&dir).output()?;
        if !out.status.success() {
            return Err(anyhow!("tar 展開失敗: {}", String::from_utf8_lossy(&out.stderr)));
        }
    }
    fs::remove_file(&archive).ok();

    #[cfg(unix)]
    {
        if let Some(exe) = find_exe_recursive(&dir) {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = fs::metadata(&exe)?.permissions();
            perm.set_mode(0o755);
            fs::set_permissions(&exe, perm)?;
        }
    }

    let exe = find_exe_recursive(&dir).ok_or_else(|| anyhow!("展開先に llama-server が見つかりません"))?;
    fs::write(dir.join(".loai-ok"), b"ok")?;
    tui::succ(&format!("llama.cpp {} ({}) をインストールしました", asset.tag, backend.name()));
    Ok((asset.tag, exe))
}

/// Ensure an engine exists for the requested backend, with fallbacks.
pub fn ensure_engine(preferred: Backend) -> Result<(Backend, String, PathBuf)> {
    let order: Vec<Backend> = match preferred {
        Backend::Vulkan => vec![Backend::Vulkan, Backend::Cpu],
        Backend::Sycl => vec![Backend::Sycl, Backend::Vulkan, Backend::Cpu],
        Backend::Cpu => vec![Backend::Cpu],
    };
    let mut last_err = None;
    for b in order {
        match install_engine(b) {
            Ok((tag, exe)) => return Ok((b, tag, exe)),
            Err(e) => {
                tui::warn(&format!("{} エンジンの準備に失敗: {e:#}", b.name()));
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("エンジンを準備できませんでした")))
}

pub fn engine_dir(tag: &str, backend: Backend) -> PathBuf {
    config::engines_dir().join(format!("llama-{tag}-{}", backend.name()))
}

fn free_port() -> Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    drop(l);
    Ok(port)
}

/// Handle to a running llama-server. Killed on drop.
pub struct RunningServer {
    pub child: Child,
    pub port: u16,
    pub backend: Backend,
    pub log_path: PathBuf,
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(config::pid_file());
    }
}

struct LaunchAttempt {
    ngl: u32,
    fa: FaStyle,
    kv_quant: Option<String>,
    thinking_kwargs: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum FaStyle {
    /// new llama.cpp syntax: -fa on (required since ~b5600 for value-taking -fa)
    On,
    /// legacy boolean flag: -fa
    Bare,
    /// no flash attention (KV stays fp16)
    None,
}

#[derive(Default, Clone)]
pub struct LaunchOverrides {
    pub ctx: Option<u32>,
    pub threads: Option<u32>,
    pub ngl: Option<u32>,
    pub kv_quant: Option<String>,
}

/// Launch llama-server for a profile with the fallback ladder:
///   1. full profile tuning (fa + kv quant + thinking kwargs)
///   2. without chat-template kwargs
///   3. without fa / kv quantization
///   4. ngl 0 (CPU-style offload)
/// If backend is GPU and everything failed, retry the ladder on CPU.
pub fn launch(
    profile: &Profile,
    backend: Backend,
    enable_thinking: bool,
    ov: &LaunchOverrides,
) -> Result<RunningServer> {
    let (_tag, exe) = install_engine(backend)?;
    let model_path = profile.local_path();
    if !model_path.exists() {
        return Err(anyhow!(
            "モデルファイルがありません: {}\n`loai models pull {}` を先に実行してください",
            model_path.display(),
            profile.id
        ));
    }

    let mut backends: Vec<Backend> = vec![backend];
    if backend.has_gpu() {
        backends.push(Backend::Cpu);
    }

    let mut last_err: Option<anyhow::Error> = None;
    for b in &backends {
        let ladder = build_ladder(profile, *b, enable_thinking);
        for attempt in &ladder {
            match spawn_and_wait(exe.clone(), profile, *b, attempt, ov) {
                Ok(srv) => {
                    if attempt.fa == FaStyle::None && profile.tuning.fa {
                        tui::warn("フル設定での起動に失敗したため、簡易設定 (KV量子化なし) で起動しました");
                    }
                    if *b != backend {
                        tui::warn(&format!("{} では起動できなかったため、CPU バックエンドに切り替えました", backend.name()));
                    }
                    return Ok(srv);
                }
                Err(e) => {
                    tui::warn(&format!("起動失敗 ({}, ngl={}): {e:#}", b.name(), attempt.ngl));
                    last_err = Some(e);
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("llama-server を起動できませんでした")))
}

fn build_ladder(profile: &Profile, backend: Backend, enable_thinking: bool) -> Vec<LaunchAttempt> {
    let ngl_auto: u32 = if backend.has_gpu() { 999 } else { 0 };
    let base_ngl = profile.tuning.ngl.unwrap_or(ngl_auto);
    let use_kwargs = profile.tuning.hybrid_thinking && !enable_thinking;
    let fa = if profile.tuning.fa { FaStyle::On } else { FaStyle::None };
    let kv = profile.tuning.kv_quant.clone();
    let mut out = vec![
        // full tuning: FA on + KV quant (+ no-think kwargs)
        LaunchAttempt { ngl: base_ngl, fa, kv_quant: kv.clone(), thinking_kwargs: use_kwargs },
        LaunchAttempt { ngl: base_ngl, fa, kv_quant: kv.clone(), thinking_kwargs: false },
        // legacy -fa flag (older llama.cpp builds)
        LaunchAttempt { ngl: base_ngl, fa: FaStyle::Bare, kv_quant: kv.clone(), thinking_kwargs: false },
        // no FA / no KV quant
        LaunchAttempt { ngl: base_ngl, fa: FaStyle::None, kv_quant: None, thinking_kwargs: false },
    ];
    if base_ngl > 0 {
        out.push(LaunchAttempt { ngl: 0, fa: FaStyle::None, kv_quant: None, thinking_kwargs: false });
    }
    out
}

fn log_tail(path: &Path, lines: usize) -> String {
    fs::read_to_string(path)
        .map(|t| {
            t.lines()
                .rev()
                .take(lines)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn spawn_and_wait(
    exe: PathBuf,
    profile: &Profile,
    backend: Backend,
    attempt: &LaunchAttempt,
    ov: &LaunchOverrides,
) -> Result<RunningServer> {
    let port = free_port()?;
    config::ensure_dirs()?;
    let log_path = config::logs_dir().join("llama-server.log");
    let log_file = fs::File::create(&log_path).context("ログファイルを作成できません")?;
    let log_err = log_file.try_clone().context("ログ複製に失敗")?;

    let ctx = ov.ctx.unwrap_or(profile.tuning.ctx);
    let threads = ov.threads.unwrap_or(crate::util::llama_threads() as u32);
    let ngl = if attempt.ngl == 999 && !backend.has_gpu() { 0 } else { attempt.ngl };
    let kv = ov.kv_quant.clone().or_else(|| attempt.kv_quant.clone());

    let mut args: Vec<String> = vec![
        "-m".into(),
        profile.local_path().to_string_lossy().into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "-c".into(),
        ctx.to_string(),
        "-t".into(),
        threads.to_string(),
        "-ngl".into(),
        ngl.to_string(),
    ];
    if profile.tuning.jinja {
        args.push("--jinja".into());
    }
    match attempt.fa {
        FaStyle::On => {
            args.push("-fa".into());
            args.push("on".into());
        }
        FaStyle::Bare => args.push("-fa".into()),
        FaStyle::None => {}
    }
    if let Some(kvq) = &kv {
        args.push("--cache-type-k".into());
        args.push(kvq.clone());
        args.push("--cache-type-v".into());
        args.push(kvq.clone());
    }
    if attempt.thinking_kwargs {
        args.push("--chat-template-kwargs".into());
        args.push(r#"{"enable_thinking": false}"#.into());
    }
    args.push("--no-webui".into());

    let mut cmd = Command::new(&exe);
    cmd.args(&args)
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().with_context(|| format!("{} を起動できません", exe.display()))?;

    // wait for /health
    let size_gb = profile.weights_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let timeout = Duration::from_secs(45 + (size_gb * 8.0) as u64);
    let start = Instant::now();
    let client = crate::download::dl_agent(10);
    loop {
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(
                "タイムアウト ({:.0}s) — モデルのロードが完了しません。RAM 不足の可能性があります (`loai doctor`):\nログ: {}",
                timeout.as_secs_f64(),
                log_path.display()
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = child.kill();
                let tail = log_tail(&log_path, 12);
                return Err(anyhow!("llama-server が異常終了 (exit {status})\n{tail}"));
            }
            Ok(None) => {}
            Err(e) => return Err(anyhow!("プロセス監視エラー: {e}")),
        }
        if let Ok(resp) = client.get(&format!("http://127.0.0.1:{port}/health")).call() {
            if resp.status() == 200 {
                let body = resp.into_string().unwrap_or_default();
                if body.contains("\"ok\"") {
                    fs::write(config::pid_file(), child.id().to_string()).ok();
                    tui::info(&format!(
                        "llama-server 起動完了 [{}] http://127.0.0.1:{port} (ctx={}, kv={}, {}, threads={})",
                        backend.name(),
                        ctx,
                        kv.as_deref().unwrap_or("fp16"),
                        if ngl > 0 { "GPUオフロード" } else { "CPU" },
                        threads
                    ));
                    return Ok(RunningServer { child, port, backend, log_path });
                }
            }
        }
        std::thread::sleep(Duration::from_millis(700));
    }
}

/// Kill a stray llama-server recorded in the pid file.
pub fn stop_server() -> Result<()> {
    let pid = fs::read_to_string(config::pid_file())
        .context("pid ファイルがありません (稼働中の llama-server は未検出)")?
        .trim()
        .to_string();
    let pid: u32 = pid.parse().context("pid ファイルが壊れています")?;
    #[cfg(unix)]
    {
        let out = Command::new("kill").arg("-9").arg(pid.to_string()).output()?;
        if !out.status.success() {
            return Err(anyhow!("kill {pid} に失敗"));
        }
    }
    #[cfg(windows)]
    {
        let out = Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).output()?;
        if !out.status.success() {
            return Err(anyhow!("taskkill /PID {pid} に失敗"));
        }
    }
    fs::remove_file(config::pid_file()).ok();
    tui::succ(&format!("llama-server (PID {pid}) を停止しました"));
    Ok(())
}

/// Quick throughput benchmark against a running server.
pub struct BenchResult {
    pub ttft_ms: u128,
    pub tokens: usize,
    pub tok_per_sec: f64,
}

pub fn bench_once(port: u16) -> Result<BenchResult> {
    let body = serde_json::json!({
        "model": "bench",
        "messages": [{"role": "user", "content": "Write a bubble sort function in Rust. Code only, no explanation."}],
        "max_tokens": 128,
        "temperature": 0.1,
        "stream": true
    });
    let agent = crate::download::dl_agent(300);
    let resp = agent
        .post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| anyhow!("ベンチマークリクエスト失敗: {e}"))?;
    let reader = BufReader::new(resp.into_reader());
    let start = Instant::now();
    let mut ttft: Option<u128> = None;
    let mut tokens = 0usize;
    for line in reader.lines() {
        let line = line?;
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
            if let Some(content) = v.pointer("/choices/0/delta/content").and_then(|c| c.as_str()) {
                if !content.is_empty() && ttft.is_none() {
                    ttft = Some(start.elapsed().as_millis());
                }
                tokens += 1; // delta chunks ≈ tokens
            }
            if let Some(u) = v.pointer("/usage/completion_tokens").and_then(|t| t.as_u64()) {
                tokens = u as usize;
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64().max(0.001);
    let gen_time = (elapsed - ttft.unwrap_or(0) as f64 / 1000.0).max(0.001);
    let tps = tokens as f64 / gen_time;
    Ok(BenchResult { ttft_ms: ttft.unwrap_or(0), tokens, tok_per_sec: tps })
}
