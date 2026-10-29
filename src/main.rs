//! loai — 極限最適化ローカルコーディングエージェント
//! 16GB RAM + Intel iGPU (Iris Xe) 向けにllama.cppと量子化モデルを自動チューニングする。

mod agent;
mod chat;
mod config;
mod doctor;
mod download;
mod engine;
mod llm;
mod models;
mod split;
mod tui;
mod util;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "loai",
    version,
    about = "極限最適化ローカルコーディングエージェント (16GB RAM + Intel iGPU 向け)",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// llama.cpp エンジンをインストール (Vulkan + CPU)
    Install {
        #[arg(long, default_value = "auto")]
        backend: String,
    },
    /// ハードウェア診断とモデル推奨
    Doctor,
    /// 現在のモデルで生成速度を測定 (--apply で最速バックエンドを保存)
    Bench {
        #[arg(long)]
        apply: bool,
    },
    /// チャット (ツールなし)
    Chat {
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        api_base: Option<String>,
        #[arg(long)]
        no_think: bool,
    },
    /// 自律コーディングエージェント
    Agent {
        /// 実行するタスク
        task: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value_t = 24)]
        max_turns: u32,
        /// すべてのツール実行を自動承認 (危険)
        #[arg(long)]
        yes: bool,
        /// 思考モードを有効化 (遅いが高品質)
        #[arg(long)]
        think: bool,
        /// 思考プロセスをリアルタイム表示
        #[arg(long)]
        reasoning: bool,
        #[arg(long)]
        api_base: Option<String>,
    },
    /// モデルの管理
    Models {
        #[command(subcommand)]
        sub: ModelsCmd,
    },
    /// エンジンの管理
    Engine {
        #[command(subcommand)]
        sub: EngineCmd,
    },
    /// 大容量ファイルを GitHub Releases 用に 2GB 分割
    Split {
        file: String,
        #[arg(long, default_value_t = 1900)]
        chunk_mb: u64,
    },
    /// 分割パーツを結合 (マニフェストを指定)
    Join {
        manifest: String,
        #[arg(long)]
        out: Option<String>,
    },
    /// 稼働中の llama-server を停止
    Stop,
}

#[derive(Subcommand)]
enum ModelsCmd {
    /// カタログ一覧
    List,
    /// モデルをダウンロード (HF優先 / GHミラーフォールバック)
    Pull { id: String },
    /// モデルを削除
    Rm { id: String },
    /// モデルファイルのパス表示
    Path { id: String },
    /// sha256 で完全検証
    Verify { id: String },
    /// 既定のモデルを設定
    Use { id: String },
}

#[derive(Subcommand)]
enum EngineCmd {
    /// インストール済みエンジン一覧
    List,
    /// エンジンのパス表示
    Path,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = dispatch(cli.cmd) {
        tui::err(&format!("{e:#}"));
        std::process::exit(1);
    }
}

fn dispatch(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Install { backend } => cmd_install(&backend),
        Cmd::Doctor => doctor::run(),
        Cmd::Bench { apply } => cmd_bench(apply),
        Cmd::Chat { model, api_base, no_think } => chat::run(model.as_deref(), api_base.as_deref(), no_think),
        Cmd::Agent { task, model, max_turns, yes, think, reasoning, api_base } => agent::run(
            &task,
            model.as_deref(),
            agent::AgentOptions { max_turns, auto_yes: yes, show_reasoning: reasoning, think },
            api_base.as_deref(),
        ),
        Cmd::Models { sub } => cmd_models(sub),
        Cmd::Engine { sub } => cmd_engine(sub),
        Cmd::Split { file, chunk_mb } => {
            let p = PathBuf::from(&file);
            split::split_file(&p, chunk_mb * 1024 * 1024)?;
            Ok(())
        }
        Cmd::Join { manifest, out } => {
            let m = PathBuf::from(&manifest);
            split::join_parts(&m, out.as_deref().map(PathBuf::from).as_deref())?;
            Ok(())
        }
        Cmd::Stop => engine::stop_server(),
    }
}

fn cmd_install(backend: &str) -> Result<()> {
    config::ensure_dirs()?;
    let b = engine::Backend::from_str(backend)?;
    let order: Vec<engine::Backend> = match b {
        engine::Backend::Vulkan => vec![engine::Backend::Vulkan, engine::Backend::Cpu],
        engine::Backend::Sycl => vec![engine::Backend::Sycl, engine::Backend::Cpu],
        engine::Backend::Cpu => vec![engine::Backend::Cpu],
    };
    for be in &order {
        let _ = engine::install_engine(*be);
    }
    let installed: Vec<String> = order
        .iter()
        .filter_map(|b| engine::find_installed(*b).map(|_| b.name().to_string()))
        .collect();
    if installed.is_empty() {
        return Err(anyhow!("エンジンのインストールに失敗しました。ネットワークと gh 認証を確認してください"));
    }
    tui::succ(&format!("利用可能バックエンド: {}", installed.join(", ")));
    println!();
    println!("{}", tui::dim("次: loai doctor で環境診断 → loai models pull qwen3-8b"));
    Ok(())
}

fn cmd_models(sub: ModelsCmd) -> Result<()> {
    let cat = models::Catalog::load()?;
    match sub {
        ModelsCmd::List => {
            println!("{}", tui::cyan("── loai モデルカタログ ──"));
            println!();
            for p in &cat.profiles {
                let pulled = if p.is_pulled() { tui::green("[DL済]") } else { tui::dim("[未DL]") };
                let exp = if p.experimental { tui::yellow(" (実験)") } else { String::new() };
                println!("{} {:<22} {} {}{}  必要RAM: {}GB / ファイル: {}",
                    pulled, p.id, p.name, crate::util::human_bytes(p.hf.size), exp, p.min_ram_gb, p.hf.file);
                println!("    {}", tui::dim(&p.desc));
            }
            Ok(())
        }
        ModelsCmd::Pull { id } => {
            let p = cat.find(&id)?;
            tui::info(&format!("{} を取得します ({})", p.id, crate::util::human_bytes(p.hf.size)));
            let path = download::pull_model(p)?;
            tui::succ(&format!("モデル配置完了: {}", path.display()));
            println!();
            println!("{}", tui::dim("次: loai agent \"タスク\" または loai chat"));
            Ok(())
        }
        ModelsCmd::Rm { id } => {
            let p = cat.find(&id)?;
            let path = p.local_path();
            if path.exists() {
                std::fs::remove_file(&path)?;
                tui::succ(&format!("削除しました: {}", path.display()));
            } else {
                tui::warn("ファイルが存在しません");
            }
            Ok(())
        }
        ModelsCmd::Path { id } => {
            let p = cat.find(&id)?;
            println!("{}", p.local_path().display());
            Ok(())
        }
        ModelsCmd::Verify { id } => {
            let p = cat.find(&id)?;
            let path = p.local_path();
            if !path.exists() {
                return Err(anyhow!("未ダウンロードです。`loai models pull {}` を先に実行", p.id));
            }
            tui::info("sha256 を検証しています...");
            let got = download::sha256_file(&path)?;
            if got == p.hf.sha256 {
                tui::succ("検証OK — ファイルは完全です");
            } else {
                return Err(anyhow!("破損しています (期待 {} / 実際 {})\n`loai models rm {}` 後に再ダウンロードしてください", p.hf.sha256, got, p.id));
            }
            Ok(())
        }
        ModelsCmd::Use { id } => {
            let p = cat.find(&id)?;
            let mut cfg = config::load_config();
            cfg.model = Some(p.id.clone());
            config::save_config(&cfg)?;
            tui::succ(&format!("既定モデルを {} に設定しました", p.id));
            Ok(())
        }
    }
}

fn cmd_engine(sub: EngineCmd) -> Result<()> {
    match sub {
        EngineCmd::List => {
            for b in [engine::Backend::Vulkan, engine::Backend::Cpu, engine::Backend::Sycl] {
                match engine::find_installed(b) {
                    Some((tag, exe)) => println!("{:<8} llama-{} → {}", b.name(), tag, exe.display()),
                    None => println!("{:<8} (未インストール)", b.name()),
                }
            }
            Ok(())
        }
        EngineCmd::Path => {
            println!("{}", config::engines_dir().display());
            Ok(())
        }
    }
}

fn cmd_bench(apply: bool) -> Result<()> {
    let cfg = config::load_config();
    let cat = models::Catalog::load()?;
    let profile = cat.resolve(None)?;
    if !profile.is_pulled() {
        return Err(anyhow!("モデル {} が未ダウンロードです: loai models pull {}", profile.id, profile.id));
    }
    let backend = match cfg.backend.as_deref() {
        Some(b) => engine::Backend::from_str(b)?,
        None => engine::Backend::Vulkan,
    };
    let ov = engine::LaunchOverrides {
        ctx: cfg.ctx,
        threads: cfg.threads,
        ngl: cfg.ngl,
        kv_quant: cfg.kv_quant.clone(),
    };
    let server = engine::launch(profile, backend, false, &ov)?;
    let server_backend = server.backend;
    let server_name = server.backend.name().to_string();
    tui::info("ベンチマーク実行中 (128トークン生成)...");
    let r = engine::bench_once(server.port)?;
    drop(server);
    println!();
    println!("{}", tui::cyan("── ベンチマーク結果 ──"));
    println!("  モデル        : {} ({})", profile.id, profile.name);
    println!("  バックエンド  : {}", server_name);
    println!("  初頭応答(TTFT): {} ms", r.ttft_ms);
    println!("  生成速度      : {:.1} token/s", r.tok_per_sec);
    println!("  体感目安      : {}",
        if r.tok_per_sec >= 20.0 { "軽快 (高速プロファイル相当)" }
        else if r.tok_per_sec >= 8.0 { "実用的 (コーディング可能)" }
        else { "低速 (より軽いモデルを推奨)" });

    // compare with CPU if we used a GPU backend
    if server_backend.has_gpu() {
        tui::info("CPUバックエンドでも比較測定します...");
        let cpu_server = engine::launch(profile, engine::Backend::Cpu, false, &ov)?;
        let r2 = engine::bench_once(cpu_server.port)?;
        println!("  (CPU比較)     : {:.1} token/s", r2.tok_per_sec);
        let gpu_wins = r.tok_per_sec >= r2.tok_per_sec;
        let chosen = if gpu_wins { server_backend } else { engine::Backend::Cpu };
        println!();
        if apply {
            let mut c = config::load_config();
            c.backend = Some(chosen.name().to_string());
            config::save_config(&c)?;
            tui::succ(&format!("最速バックエンド {} を既定に保存しました", chosen.name()));
        } else {
            tui::info(&format!("推奨: {} — 保存するには --apply を付けて再実行", chosen.name()));
        }
    }
    Ok(())
}
