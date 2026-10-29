//! Minimal OpenAI-compatible chat client with SSE streaming and tool-call
//! accumulation (works with llama-server, LM Studio, Ollama v1 endpoints...).

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};

#[derive(Debug, Clone, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Default)]
pub struct StreamOutcome {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage_prompt: Option<u64>,
    pub usage_completion: Option<u64>,
}

#[derive(Clone)]
pub struct Client {
    pub base: String,
    pub api_key: Option<String>,
}

impl Client {
    pub fn new(base: &str, api_key: Option<String>) -> Self {
        let base = base.trim_end_matches('/').to_string();
        Client { base, api_key }
    }

    pub fn is_local(&self) -> bool {
        self.base.contains("127.0.0.1") || self.base.contains("localhost")
    }

    /// POST /v1/chat/completions with stream, invoking callbacks per delta.
    pub fn chat_stream(
        &self,
        model: &str,
        messages: &[Value],
        tools: &[Value],
        temperature: f32,
        mut on_delta: impl FnMut(&str, &str),
    ) -> Result<StreamOutcome> {
        let mut body = json!({
            "model": model,
            "messages": messages,
            "temperature": temperature,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }

        let agent = crate::download::dl_agent(600);
        let mut req = agent
            .post(&format!("{}/chat/completions", self.base))
            .set("Content-Type", "application/json");
        if let Some(k) = &self.api_key {
            req = req.set("Authorization", &format!("Bearer {k}"));
        }
        let resp = match req.send_json(&body) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                return Err(anyhow!("API エラー {code}: {text}"));
            }
            Err(e) => return Err(anyhow!("推論サーバーに接続できません: {e}")),
        };

        let reader = BufReader::new(resp.into_reader());
        let mut out = StreamOutcome::default();
        let mut pending: Vec<ToolCall> = Vec::new();

        for line in reader.lines() {
            let line = line?;
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };

            if let Some(err) = v.get("error") {
                return Err(anyhow!("推論サーバーエラー: {err}"));
            }
            if let Some(choice) = v.pointer("/choices/0").filter(|c| c.is_object()) {
                if let Some(delta) = choice.get("delta") {
                    if let Some(c) = delta.get("content").and_then(|x| x.as_str()) {
                        if !c.is_empty() {
                            out.content.push_str(c);
                            on_delta(c, "");
                        }
                    }
                    if let Some(r) = delta.get("reasoning_content").and_then(|x| x.as_str()) {
                        if !r.is_empty() {
                            out.reasoning.push_str(r);
                            on_delta("", r);
                        }
                    }
                    if let Some(tcs) = delta.get("tool_calls").and_then(|x| x.as_array()) {
                        for tc in tcs {
                            let idx = tc.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                            while pending.len() <= idx {
                                pending.push(ToolCall::default());
                            }
                            let e = &mut pending[idx];
                            if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
                                if !id.is_empty() {
                                    e.id = id.to_string();
                                }
                            }
                            if let Some(f) = tc.get("function") {
                                if let Some(nm) = f.get("name").and_then(|x| x.as_str()) {
                                    if !nm.is_empty() {
                                        e.name.push_str(nm);
                                    }
                                }
                                if let Some(a) = f.get("arguments").and_then(|x| x.as_str()) {
                                    e.arguments.push_str(a);
                                }
                            }
                        }
                    }
                }
                if let Some(fr) = choice.get("finish_reason").and_then(|x| x.as_str()) {
                    out.finish_reason = Some(fr.to_string());
                }
            }
            if let Some(p) = v.pointer("/usage/prompt_tokens").and_then(|x| x.as_u64()) {
                out.usage_prompt = Some(p);
            }
            if let Some(c) = v.pointer("/usage/completion_tokens").and_then(|x| x.as_u64()) {
                out.usage_completion = Some(c);
            }
        }

        // fallback: some models emit Hermes-style <tool_call/> in plain content
        if pending.iter().all(|t| t.name.is_empty()) {
            let parsed = fallback_parse_tool_calls(&out.content);
            if !parsed.is_empty() {
                pending = parsed;
            }
        }
        pending.retain(|t| !t.name.is_empty());
        for (i, t) in pending.iter_mut().enumerate() {
            if t.id.is_empty() {
                t.id = format!("call_{i}");
            }
            if !t.arguments.trim().starts_with('{') {
                t.arguments = "{}".to_string();
            }
        }
        out.tool_calls = pending;
        Ok(out)
    }
}

/// Parse Qwen-Hermes style tool calls from raw text. Tolerates
///   <tool_calljson>{...}</tool_calljson>  and  <tool_calljson>{...}</tool_calljson>
/// (with/without the `json` suffix), plus fenced ```json blocks.
pub fn fallback_parse_tool_calls(text: &str) -> Vec<ToolCall> {
    let mut out = Vec::new();
    let re = regex::Regex::new(r"(?s)<tool_call(?:json)?\s*>?\s*(\{.*\})\s*</tool_call(?:json)?>").unwrap();
    for cap in re.captures_iter(text) {
        if let Some(tc) = parse_tc_json(&cap[1]) {
            out.push(tc);
        }
    }
    if out.is_empty() {
        // fenced json
        let re2 = regex::Regex::new(r#"(?s)```(?:json)?\s*(\{\s*"name"\s*:.*?\})\s*```"#).unwrap();
        for cap in re2.captures_iter(text) {
            if let Some(tc) = parse_tc_json(&cap[1]) {
                out.push(tc);
            }
        }
    }
    out
}

fn parse_tc_json(s: &str) -> Option<ToolCall> {
    let v: Value = serde_json::from_str(s.trim()).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    let arguments = match v.get("arguments") {
        Some(a) => serde_json::to_string(a).unwrap_or_else(|_| "{}".into()),
        None => "{}".into(),
    };
    Some(ToolCall { id: String::new(), name, arguments })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fallback_hermes() {
        let text = r#"やります。<tool_calljson>
{"name": "read_file", "arguments": {"path": "src/main.rs"}}
</tool_calljson>"#;
        let tcs = fallback_parse_tool_calls(text);
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].name, "read_file");
        let args: Value = serde_json::from_str(&tcs[0].arguments).unwrap();
        assert_eq!(args["path"], "src/main.rs");
    }

    #[test]
    fn test_fallback_no_space() {
        let text = r#"<tool_call{"name":"list_files","arguments":{}}</tool_calljson>"#;
        let tcs = fallback_parse_tool_calls(text);
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].name, "list_files");
    }

    #[test]
    fn test_no_false_positive() {
        let text = "普通のテキストです。tool_call という単語だけ。";
        assert!(fallback_parse_tool_calls(text).is_empty());
    }
}
