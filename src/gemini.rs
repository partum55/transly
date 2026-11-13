use base64::Engine;
use serde_json::{Value, json};
use std::time::Duration;

// Flash-Lite tier: most free-tier headroom, and not a moving "-latest" alias that gets overloaded.
const MODEL: &str = "gemini-3.5-flash-lite";
const MAX_PDF_BYTES: usize = 18 * 1024 * 1024; // inline request limit is ~20MB

/// (display name, file-name code)
pub const LANGS: &[(&str, &str)] = &[
    ("English", "en"),
    ("German", "de"),
    ("French", "fr"),
    ("Spanish", "es"),
    ("Italian", "it"),
    ("Portuguese", "pt"),
    ("Dutch", "nl"),
    ("Polish", "pl"),
    ("Ukrainian", "uk"),
    ("Russian", "ru"),
    ("Turkish", "tr"),
    ("Chinese (Simplified)", "zh"),
    ("Japanese", "ja"),
    ("Korean", "ko"),
];

const RULES: &str = "You are a professional translator. Detect the source language automatically. \
Translate faithfully into {lang}. Keep meaning, tone, line breaks, markdown, numbers, URLs, \
code and timestamps unchanged. If the text is already in {lang}, return it unchanged. \
Output only the translation, no commentary.";

fn system(lang: &str) -> Value {
    json!({ "parts": [{ "text": RULES.replace("{lang}", lang) }] })
}

fn call(key: &str, body: Value) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .into();
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{MODEL}:generateContent");
    let mut resp = agent
        .post(&url)
        .header("x-goog-api-key", key)
        .send_json(&body)
        .map_err(|e| format!("Network error: {e}"))?;
    let status = resp.status().as_u16();
    let v: Value = resp
        .body_mut()
        .read_json()
        .map_err(|e| format!("Bad response from Gemini: {e}"))?;

    if status != 200 {
        let msg = v["error"]["message"].as_str().unwrap_or("unknown error");
        return Err(match status {
            429 => "Free-tier quota reached. Wait a bit (or until tomorrow) and try again.".into(),
            400 | 403 if msg.contains("API key") => "The API key was rejected. Check it in Settings.".into(),
            _ => format!("Gemini error {status}: {msg}"),
        });
    }

    let cand = &v["candidates"][0];
    let text: String = cand["content"]["parts"]
        .as_array()
        .map(|p| p.iter().filter_map(|x| x["text"].as_str()).collect())
        .unwrap_or_default();
    match cand["finishReason"].as_str() {
        Some("MAX_TOKENS") => Err("Response was cut off (too long). Try a smaller input.".into()),
        _ if text.is_empty() => {
            let why = cand["finishReason"]
                .as_str()
                .or(v["promptFeedback"]["blockReason"].as_str())
                .unwrap_or("empty response");
            Err(format!("Gemini returned nothing ({why})."))
        }
        _ => Ok(text),
    }
}

pub fn translate(key: &str, text: &str, lang: &str) -> Result<String, String> {
    call(
        key,
        json!({
            "systemInstruction": system(lang),
            "contents": [{ "parts": [{ "text": text }] }],
        }),
    )
}

/// Translates each string independently; result has the same length and order.
pub fn translate_batch(key: &str, items: &[String], lang: &str) -> Result<Vec<String>, String> {
    let input = serde_json::to_string(items).map_err(|e| e.to_string())?;
    let out = call(
        key,
        json!({
            "systemInstruction": { "parts": [{ "text": format!(
                "{} The input is a JSON array of strings. Return a JSON array of the same length, \
                 where item i is the translation of input item i.", RULES.replace("{lang}", lang)) }] },
            "contents": [{ "parts": [{ "text": input }] }],
            "generationConfig": {
                "responseMimeType": "application/json",
                "responseSchema": { "type": "ARRAY", "items": { "type": "STRING" } },
            },
        }),
    )?;
    let res: Vec<String> = serde_json::from_str(&out).map_err(|e| format!("Bad batch response: {e}"))?;
    if res.len() != items.len() {
        return Err(format!("Gemini returned {} items, expected {}.", res.len(), items.len()));
    }
    Ok(res)
}

/// Gemini reads PDFs natively; returns the translated content as Markdown.
// ponytail: one request for the whole PDF; very long PDFs can hit the output-token cap. Split by page if needed.
pub fn translate_pdf(key: &str, pdf: &[u8], lang: &str) -> Result<String, String> {
    if pdf.len() > MAX_PDF_BYTES {
        return Err("PDF is larger than 18 MB, which is too big to send.".into());
    }
    call(
        key,
        json!({
            "systemInstruction": system(lang),
            "contents": [{ "parts": [
                { "inline_data": { "mime_type": "application/pdf",
                                   "data": base64::engine::general_purpose::STANDARD.encode(pdf) } },
                { "text": format!("Translate this whole document into {lang}. Output Markdown that keeps headings (#), lists and tables. Put a blank line between every heading, paragraph, list and table, and keep each separate line of the original as its own paragraph.") },
            ]}],
        }),
    )
    .map(|md| strip_fence(&md))
}

/// Models sometimes wrap the whole answer in a ```markdown fence.
fn strip_fence(s: &str) -> String {
    let t = s.trim();
    match t.strip_prefix("```").and_then(|r| r.split_once('\n')).map(|(_, body)| body) {
        Some(body) if t.ends_with("```") => body.trim_end_matches("```").trim_end().to_string(),
        _ => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_stripping() {
        assert_eq!(strip_fence("```markdown\n# A\n\ntext\n```\n"), "# A\n\ntext");
        assert_eq!(strip_fence("# A\n\n```rust\ncode\n```\n"), "# A\n\n```rust\ncode\n```\n");
    }

    /// Live check against the real API: `GEMINI_API_KEY=... cargo test live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live() {
        let key = std::env::var("GEMINI_API_KEY").expect("set GEMINI_API_KEY");
        let one = translate(&key, "Good morning, how are you?", "German").unwrap();
        println!("single: {one}");
        let items = vec!["Hello".to_string(), "See you tomorrow & goodbye".to_string()];
        let many = translate_batch(&key, &items, "French").unwrap();
        println!("batch: {many:?}");
        assert_eq!(many.len(), 2);
    }
}
