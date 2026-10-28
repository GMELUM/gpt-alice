use serde_json::{Value, json};

use crate::oauth::OAuthController;

const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const MODEL: &str = "gpt-6-luna";
const MAX_SSE_BUFFER: usize = 4 * 1024 * 1024;
const MAX_SPOKEN_CHARS: usize = 1_800;

pub async fn search(
    client: &reqwest::Client,
    oauth: &OAuthController,
    history: &[(String, String)],
) -> Result<String, String> {
    let (token, account_id) = oauth.live_credentials().await?;
    let prompt = conversation_prompt(history)?;
    let response = client
        .post(CODEX_RESPONSES_URL)
        .timeout(std::time::Duration::from_secs(60))
        .bearer_auth(&token)
        .header("ChatGPT-Account-ID", &account_id)
        .header("originator", "codex_cli_rs")
        .header("User-Agent", concat!("gpt-alice/", env!("CARGO_PKG_VERSION")))
        .json(&json!({
            "model": MODEL,
            "store": false,
            "stream": true,
            "instructions": "You are a web research assistant for a live spoken conversation. Use current web search results, answer in the user's language, be concise, and include reliable source URLs. Distinguish verified facts from uncertainty. Never claim a search succeeded unless the web_search tool returned results.",
            "tools": [{"type":"web_search"}],
            "tool_choice": {"type":"web_search"},
            "parallel_tool_calls": false,
            "reasoning": {"effort":"low"},
            "max_output_tokens": 700,
            "input": [{
                "role": "user",
                "content": [{"type":"input_text", "text": prompt}]
            }]
        }))
        .send()
        .await
        .map_err(|error| format!("Не удалось выполнить поиск через Codex: {error}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let safe_body: String = body
            .replace(&token, "[скрыто]")
            .replace(&account_id, "[скрыто]")
            .chars()
            .take(400)
            .collect();
        return Err(if safe_body.trim().is_empty() {
            format!("Codex web search вернул HTTP {status}")
        } else {
            format!("Codex web search вернул HTTP {status}: {safe_body}")
        });
    }

    let mut response = response;
    let mut buffered = Vec::new();
    let mut deltas = String::new();
    let mut completed_text = None;
    let mut citations = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("Ошибка потока Codex web search: {error}"))?
    {
        buffered.extend_from_slice(&chunk);
        if buffered.len() > MAX_SSE_BUFFER {
            return Err("Ответ Codex web search превысил допустимый размер".into());
        }
        while let Some((start, end)) = find_sse_boundary(&buffered) {
            let block: Vec<u8> = buffered.drain(..end).collect();
            parse_sse_block(
                &block[..start],
                &mut deltas,
                &mut completed_text,
                &mut citations,
            )?;
        }
    }
    if !buffered.is_empty() {
        parse_sse_block(&buffered, &mut deltas, &mut completed_text, &mut citations)?;
    }

    let mut answer = completed_text.unwrap_or(deltas);
    if answer.trim().is_empty() {
        return Err("Codex web search завершился без текстового ответа".into());
    }
    let mut unique = Vec::new();
    for citation in citations {
        if !unique.contains(&citation) {
            unique.push(citation);
        }
        if unique.len() == 4 {
            break;
        }
    }
    if !unique.is_empty() {
        answer.push_str("\nИсточники: ");
        answer.push_str(&unique.join("; "));
    }
    Ok(answer.chars().take(MAX_SPOKEN_CHARS).collect())
}

fn conversation_prompt(history: &[(String, String)]) -> Result<String, String> {
    let latest_user = history
        .iter()
        .rev()
        .find(|(role, text)| role == "user" && !text.trim().is_empty())
        .map(|(_, text)| text.trim())
        .ok_or_else(|| "В voice-событиях не найден текст запроса для поиска".to_owned())?;
    let mut transcript = String::new();
    for (role, text) in history.iter().rev().take(12).rev() {
        let label = if role == "assistant" {
            "Ассистент"
        } else {
            "Пользователь"
        };
        let part: String = text.chars().take(1_000).collect();
        if !part.trim().is_empty() {
            transcript.push_str(label);
            transcript.push_str(": ");
            transcript.push_str(part.trim());
            transcript.push('\n');
        }
    }
    Ok(format!(
        "Последний запрос пользователя: {latest_user}\n\nНедавний контекст разговора:\n{transcript}\nНайди ответ на последний запрос с помощью web_search. Не повторяй поиск, если вопрос не требует актуальных данных или сведений с сайтов."
    ))
}

fn find_sse_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            if bytes.get(index + 1) == Some(&b'\n') {
                return Some((index, index + 2));
            }
            if bytes.get(index + 1) == Some(&b'\r') && bytes.get(index + 2) == Some(&b'\n') {
                return Some((index, index + 3));
            }
        }
        if bytes[index] == b'\r'
            && bytes.get(index + 1) == Some(&b'\n')
            && bytes.get(index + 2) == Some(&b'\r')
            && bytes.get(index + 3) == Some(&b'\n')
        {
            return Some((index, index + 4));
        }
        index += 1;
    }
    None
}

fn parse_sse_block(
    bytes: &[u8],
    deltas: &mut String,
    completed_text: &mut Option<String>,
    citations: &mut Vec<String>,
) -> Result<(), String> {
    let block = String::from_utf8_lossy(bytes);
    let data = block
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() || data == "[DONE]" {
        return Ok(());
    }
    let event: Value = serde_json::from_str(&data)
        .map_err(|error| format!("Некорректное SSE-событие Codex web search: {error}"))?;
    match event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "response.output_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                deltas.push_str(delta);
            }
        }
        "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                collect_output_item(item, completed_text, citations);
            }
        }
        "response.completed" => {
            if let Some(output) = event.pointer("/response/output").and_then(Value::as_array) {
                let mut text = String::new();
                for item in output {
                    let mut ignored_text = None;
                    collect_output_item(item, &mut ignored_text, citations);
                    if item.get("type").and_then(Value::as_str) == Some("message")
                        && let Some(content) = item.get("content").and_then(Value::as_array)
                    {
                        for part in content {
                            if part.get("type").and_then(Value::as_str) == Some("output_text")
                                && let Some(part_text) = part.get("text").and_then(Value::as_str)
                            {
                                text.push_str(part_text);
                            }
                        }
                    }
                }
                if !text.trim().is_empty() {
                    *completed_text = Some(text);
                }
            }
        }
        "response.failed" | "error" => {
            let error = event.get("error").unwrap_or(&event);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.get("code").and_then(Value::as_str))
                .unwrap_or("неизвестная ошибка");
            return Err(format!("Codex web search: {message}"));
        }
        _ => {}
    }
    Ok(())
}

fn collect_output_item(
    item: &Value,
    completed_text: &mut Option<String>,
    citations: &mut Vec<String>,
) {
    if item.get("type").and_then(Value::as_str) != Some("message") {
        return;
    }
    let Some(content) = item.get("content").and_then(Value::as_array) else {
        return;
    };
    let mut text = String::new();
    for part in content {
        if part.get("type").and_then(Value::as_str) != Some("output_text") {
            continue;
        }
        if let Some(part_text) = part.get("text").and_then(Value::as_str) {
            text.push_str(part_text);
        }
        if let Some(annotations) = part.get("annotations").and_then(Value::as_array) {
            for annotation in annotations {
                if annotation.get("type").and_then(Value::as_str) == Some("url_citation") {
                    let title = annotation
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|title| !title.is_empty())
                        .unwrap_or("Источник");
                    let url = annotation
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !url.is_empty() {
                        citations.push(format!("{title}: {url}"));
                    }
                }
            }
        }
    }
    if !text.trim().is_empty() {
        *completed_text = Some(text);
    }
}
