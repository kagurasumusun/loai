//! Pure chat REPL (no tools) for quick conversation with the local model.

use crate::agent::build_session;
use crate::tui;
use anyhow::Result;
use serde_json::{json, Value};

pub fn run(model_id: Option<&str>, api_base: Option<&str>, no_think: bool) -> Result<()> {
    let session = build_session(model_id, api_base, !no_think)?;
    let temp = session.temperature(!no_think);
    let ctx = session.ctx;
    let mut show_reasoning = !no_think;

    println!();
    println!("{}", tui::cyan("════════ loai chat ════════"));
    println!("{}", tui::dim(&format!("モデル: {} | ctx: {} | 温度: {temp}", session.model_name, ctx)));
    println!("{}", tui::dim("コマンド: /exit 終了 /clear 履歴クリア /think 思考表示切替 /save <file> 保存 /help"));
    println!();

    let system = "あなたは loai — ユーザーのPC上で動くローカルAIアシスタントです。日本語で自然に応答してください。コードは適切にマークダウンで囲みます。";
    let mut messages: Vec<Value> = vec![json!({"role": "system", "content": system})];
    let budget = (ctx as usize).saturating_sub(600);
    let stdin = std::io::stdin();

    loop {
        print!("{}", tui::green("あなた> "));
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut line = String::new();
        let n = stdin.read_line(&mut line)?;
        if n == 0 {
            tui::info("入力終了 (EOF)");
            break;
        }
        let input = line.trim().to_string();
        if input.is_empty() {
            continue;
        }
        match input.as_str() {
            "/exit" | "/quit" | "/q" => break,
            "/help" => {
                println!("{}", tui::dim("/exit 終了 /clear 履歴クリア /think 思考表示切替 /save <file> 履歴保存"));
                continue;
            }
            "/clear" => {
                messages.truncate(1);
                tui::succ("履歴をクリアしました");
                continue;
            }
            "/think" => {
                show_reasoning = !show_reasoning;
                tui::info(&format!("思考プロセス表示: {}", if show_reasoning { "ON" } else { "OFF" }));
                continue;
            }
            _ => {}
        }
        if let Some(path) = input.strip_prefix("/save ") {
            let dump = serde_json::to_string_pretty(&messages)?;
            std::fs::write(path.trim(), dump)?;
            tui::succ(&format!("{} に保存しました", path.trim()));
            continue;
        }

        messages.push(json!({"role": "user", "content": input}));

        print!("{}", tui::cyan("loai> "));
        let outcome = session.client.chat_stream(
            &session.model_name,
            &messages,
            &[],
            temp,
            |content_delta, reasoning_delta| {
                if !reasoning_delta.is_empty() && show_reasoning {
                    eprint!("{}", tui::dim(reasoning_delta));
                }
                if !content_delta.is_empty() {
                    print!("{}", content_delta);
                    std::io::stdout().flush().ok();
                }
            },
        )?;
        println!();
        if show_reasoning && !outcome.reasoning.is_empty() {
            eprintln!();
        }
        if outcome.content.trim().is_empty() && !outcome.reasoning.is_empty() {
            println!("{}", tui::dim("(思考のみで完了 — /think で内容を確認できます)"));
        }
        messages.push(json!({"role": "assistant", "content": outcome.content}));
        crate::agent::context::trim(&mut messages, budget);
    }

    tui::succ("終了します");
    Ok(())
}
