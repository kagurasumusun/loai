//! The autonomous agent loop: system prompt (Japanese, small-model tuned),
//! tool-call cycling, context trimming and final reporting.

pub mod approve;
pub mod context;
pub mod tools;

use crate::config;
use crate::engine::{self, Backend, LaunchOverrides, RunningServer};
use crate::llm::Client;
use crate::models::Catalog;
use crate::tui;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// A ready-to-use inference session (local llama.cpp or remote endpoint).
pub struct Session {
    pub client: Client,
    pub model_name: String,
    pub profile_id: Option<String>,
    pub server: Option<RunningServer>,
    pub backend: Option<Backend>,
    pub ctx: u32,
    pub family: String,
}

/// Resolve model + engine + endpoint.
pub fn build_session(model_id: Option<&str>, api_base: Option<&str>, enable_thinking: bool) -> Result<Session> {
    let cfg = config::load_config();

    // remote endpoint
    let remote_base = api_base
        .map(|s| s.to_string())
        .or_else(|| std::env::var("LOAI_API_BASE").ok())
        .or_else(|| cfg.api_base.clone());
    if let Some(base) = remote_base {
        let model_name = model_id
            .map(|s| s.to_string())
            .or_else(|| cfg.model.clone())
            .unwrap_or_else(|| "default".to_string());
        tui::info(&format!("リモートAPIを使用: {} (model: {})", base, model_name));
        let client = Client::new(&base, cfg.api_key.clone().or_else(|| std::env::var("LOAI_API_KEY").ok()));
        return Ok(Session {
            client,
            model_name,
            profile_id: None,
            server: None,
            backend: None,
            ctx: 16000,
            family: "remote".into(),
        });
    }

    // local
    let cat = Catalog::load()?;
    let profile = cat.resolve(model_id)?;
    if !profile.is_pulled() {
        return Err(anyhow!(
            "モデル {} が未ダウンロードです。\n  loai models pull {}\nを実行してください (約 {})。",
            profile.id,
            profile.id,
            crate::util::human_bytes(profile.hf.size)
        ));
    }
    let backend = match cfg.backend.as_deref() {
        Some(b) => Backend::from_str(b)?,
        None => Backend::Vulkan,
    };
    let ov = LaunchOverrides {
        ctx: cfg.ctx,
        threads: cfg.threads,
        ngl: cfg.ngl,
        kv_quant: cfg.kv_quant.clone(),
    };
    let server = engine::launch(profile, backend, enable_thinking, &ov)?;
    let port = server.port;
    Ok(Session {
        client: Client::new(&format!("http://127.0.0.1:{port}/v1"), None),
        model_name: profile.id.clone(),
        profile_id: Some(profile.id.clone()),
        server: Some(server),
        backend: Some(backend),
        ctx: ov.ctx.unwrap_or(profile.tuning.ctx),
        family: profile.family.clone(),
    })
}

impl Session {
    pub fn temperature(&self, thinking: bool) -> f32 {
        match self.family.as_str() {
            "gpt-oss" => 1.0,
            "qwen3" => {
                if thinking { 0.6 } else { 0.7 }
            }
            _ => 0.3,
        }
    }
}

pub struct AgentOptions {
    pub max_turns: u32,
    pub auto_yes: bool,
    pub show_reasoning: bool,
    pub think: bool,
}

pub fn system_prompt(root: &Path, repo_map: &str) -> String {
    format!(
        r#"あなたは loai — ユーザーのPC上で完結する自律コーディングエージェントです。

# 行動規範
1. 変更前に必ず対象を確認する (read_file / search_files / list_files)
2. 既存ファイルの修正には edit_file を使う (old_text は read_file の表示から正確にコピーする。行番号は含めない)
3. write_file は新規ファイル作成か意図的な全書き換えのみに使う
4. 修正後は run_command でビルド・テスト・実行して検証する。エラーが出たら原因を調べて修正を繰り返す
5. 破壊的操作 (大量削除・強制push等) は行わない。外部ネットワークアクセスもしない
6. タスク完了時、または続行不可能な場合は必ず finish ツールを呼び、結果を要約する
7. 1ターンで呼ぶツールは最大3つ。説明は簡潔に。作業の要点のみ報告する

# 環境
- OS: {os}
- ワークスペース: {root}

# ワークスペース構成
{repo_map}"#,
        os = std::env::consts::OS,
        root = root.display(),
    )
}

pub fn run(task: &str, model_id: Option<&str>, opts: AgentOptions, api_base: Option<&str>) -> Result<()> {
    let cwd: PathBuf = std::env::current_dir()
        .map(|p| p.canonicalize().unwrap_or(p))
        .unwrap_or_else(|_| PathBuf::from("."));
    let session = build_session(model_id, api_base, opts.think)?;
    let temp = session.temperature(opts.think);
    let ctx = session.ctx;

    println!();
    println!("{}", tui::cyan("════════ loai agent ════════"));
    println!(
        "{}",
        tui::dim(&format!(
            "モデル: {} | ctx: {} | バックエンド: {} | 温度: {temp} | 最大ターン: {}",
            session.model_name,
            ctx,
            session.backend.map(|b| b.name()).unwrap_or("remote"),
            opts.max_turns
        ))
    );
    println!("{}", tui::dim("停止: Ctrl+C が効かない場合は `loai stop`"));
    println!();

    let repo_map = context::build_repo_map(&cwd, 9000);
    let system = system_prompt(&cwd, &repo_map);
    let mut messages: Vec<Value> = vec![json!({"role": "system", "content": system}), json!({"role": "user", "content": task})];
    let tools = tools::tool_defs();
    let budget = (ctx as usize).saturating_sub(2000);

    let mut env = tools::ToolEnv::new(&cwd, opts.auto_yes);
    let mut approve = approve::interactive_approver;

    for turn in 1..=opts.max_turns {
        // buffered generation: small models often emit tool calls as text,
        // so we collect the whole turn before deciding what to display.
        eprint!("{}", tui::dim("·· 生成中\r"));
        let outcome = session.client.chat_stream(
            &session.model_name,
            &messages,
            &tools,
            temp,
            |_content_delta, _reasoning_delta| {},
        )?;
        eprint!("\r            \r");

        let display = if outcome.tool_calls.is_empty() {
            outcome.content.trim().to_string()
        } else {
            // strip fenced tool-call JSON blocks the model leaked into content
            strip_fenced_blocks(&outcome.content).trim().to_string()
        };
        if !display.is_empty() {
            println!("{}", display);
        }
        if opts.show_reasoning && !outcome.reasoning.is_empty() {
            println!("{}", tui::dim(&outcome.reasoning));
        }

        if outcome.tool_calls.is_empty() {
            // no tool call → treat as final answer
            if outcome.content.trim().is_empty() && outcome.reasoning.is_empty() {
                tui::warn("モデルが空の応答を返しました (ctx 超過の可能性)。`loai doctor` で確認してください。");
            } else if !outcome.content.trim().is_empty() {
                println!();
                println!("{}", tui::green("―― 会話として完了 ――"));
            }
            print_usage(turn, &outcome, budget, &messages);
            return Ok(());
        }

        // assistant message with tool_calls
        let content_val = if outcome.content.is_empty() { Value::Null } else { json!(outcome.content) };
        let assistant = json!({
            "role": "assistant",
            "content": content_val,
            "tool_calls": outcome
                .tool_calls
                .iter()
                .map(|tc| json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {"name": tc.name, "arguments": tc.arguments}
                }))
                .collect::<Vec<_>>(),
        });
        messages.push(assistant);

        let mut finished: Option<String> = None;
        let mut aborted = false;

        for tc in outcome.tool_calls.iter().take(3) {
            println!();
            println!("{}", tui::cyan(&format!("● {}", tc.name)));
            let args: Value = serde_json::from_str(&tc.arguments).unwrap_or_else(|_| json!({}));
            if !args.is_null() && !args.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                let compact = serde_json::to_string(&args).unwrap_or_default();
                println!("{}", tui::dim(&crate::util::truncate_chars(&compact, 240, "引数省略")));
            }

            let result = match tools::execute(&mut env, &tc.name, &args, &mut approve) {
                Ok(r) => r,
                Err(e) => tools::ToolOutcome::Text(format!("ERROR: {e:#}")),
            };

            match result {
                tools::ToolOutcome::Text(text) => {
                    // preview for the human (dim), full text goes to the model
                    print_result_preview(&text);
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tc.id,
                        "content": text,
                    }));
                }
                tools::ToolOutcome::Finish(summary) => {
                    println!();
                    println!("{}", tui::green("■ 完了"));
                    println!("{}", summary);
                    finished = Some(summary);
                    break;
                }
                tools::ToolOutcome::Abort => {
                    aborted = true;
                    break;
                }
            }
        }

        if aborted {
            tui::warn("ユーザーによって停止されました");
            return Ok(());
        }
        if let Some(summary) = finished {
            let _ = summary;
            return Ok(());
        }

        // keep context under budget before next turn
        context::trim(&mut messages, budget);
        let est = context::estimate_messages(&messages);
        println!(
            "{}",
            tui::dim(&format!("[turn {}/{} | 文脈 ~{}/{} トークン]", turn, opts.max_turns, est, ctx))
        );
    }

    tui::warn(&format!("最大ターン数 ({}) に到達しました。タスクの残りを確認してください。", opts.max_turns));
    Ok(())
}

fn print_result_preview(text: &str) {
    let mut count = 0;
    for line in text.lines() {
        if count >= 12 {
            println!("  {}", tui::dim("... (ログは省略。モデルには全文が渡されます)"));
            break;
        }
        println!("  {}", tui::dim(line));
        count += 1;
    }
}

/// Remove fenced ```json tool-call blocks from displayed content.
fn strip_fenced_blocks(text: &str) -> String {
    let re = regex::Regex::new(r"(?s)```(?:json)?\s*\{.*?\}\s*```").unwrap();
    re.replace_all(text, "").into_owned()
}

fn print_usage(turn: u32, outcome: &crate::llm::StreamOutcome, budget: usize, messages: &[Value]) {
    let est = context::estimate_messages(messages);
    let usage = match (outcome.usage_prompt, outcome.usage_completion) {
        (Some(p), Some(c)) => format!("prompt {p} / completion {c}"),
        _ => format!("~{est}/{budget} トークン (推定)"),
    };
    println!("{}", tui::dim(&format!("[{turn} ターン | 使用量: {usage}]")));
}
