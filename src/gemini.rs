use base64::Engine;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Flash-Lite tier: most free-tier headroom, and not a moving "-latest" alias that gets overloaded.
const MODEL: &str = "gemini-3.5-flash-lite";
const MAX_PDF_BYTES: usize = 18 * 1024 * 1024; // inline request limit is ~20MB

/// (display name, file-name code)
pub const LANGS: &[(&str, &str)] = &[
    ("English", "en"),
    ("Ukrainian", "uk"),
    ("German", "de"),
    ("French", "fr"),
    ("Spanish", "es"),
    ("Italian", "it"),
    ("Portuguese", "pt"),
    ("Dutch", "nl"),
    ("Polish", "pl"),
    ("Russian", "ru"),
    ("Turkish", "tr"),
    ("Chinese (Simplified)", "zh"),
    ("Japanese", "ja"),
    ("Korean", "ko"),
];

/// Translation direction by language name. `from: None` means detect automatically.
#[derive(Clone, Copy)]
pub struct Dir<'a> {
    pub from: Option<&'a str>,
    pub to: &'a str,
}

/// The tail of the part translated just before this one: (source, translation).
pub type Prev<'a> = Option<(&'a str, &'a str)>;

fn rules(dir: Dir, prev: Prev) -> String {
    let to = dir.to;
    let mut s = match dir.from {
        Some(from) => format!("You are a professional translator. Translate from {from} into {to}."),
        None => format!("You are a professional translator. Detect the source language and translate into {to}."),
    };
    s += &format!(
        " Translate faithfully and naturally: convey meaning, tone and register the way a native speaker \
         would write, not word for word. Keep line breaks, markdown, numbers, URLs, code and timestamps \
         unchanged. If the text is already in {to}, return it unchanged. Output only the translation, no commentary."
    );
    if to == "Ukrainian" {
        s += " Write natural modern Ukrainian: avoid Russianisms, surzhyk and word-for-word calques.";
    }
    if let Some((src, tr)) = prev {
        s += &format!(
            "\n\nThis text continues a longer document. Here is the part just before it, ONLY so that terms, names \
             and tone stay consistent (never translate or repeat it):\n<previous_source>\n{src}\n</previous_source>\n\
             <previous_translation>\n{tr}\n</previous_translation>"
        );
    }
    s
}

fn system(dir: Dir, prev: Prev) -> Value {
    json!({ "parts": [{ "text": rules(dir, prev) }] })
}

// ---------- rate limiting ----------
// Free tier for this model: 15 requests/min, 250K tokens/min, 500 requests/day (see AI Studio > Rate limits).

const RPM_LIMIT: usize = 14; // one below the real limit
const WINDOW: Duration = Duration::from_secs(60);
const MAX_RETRIES: u32 = 4;

static SENT: Mutex<VecDeque<Instant>> = Mutex::new(VecDeque::new());

/// Sliding window. `None`: the request fits and is recorded. `Some(wait)`: wait until the oldest one leaves the window.
fn reserve(sent: &mut VecDeque<Instant>, now: Instant, limit: usize, window: Duration) -> Option<Duration> {
    while sent.front().is_some_and(|&t| now.duration_since(t) >= window) {
        sent.pop_front();
    }
    if sent.len() < limit {
        sent.push_back(now);
        return None;
    }
    Some(window - now.duration_since(sent[0]))
}

/// Blocks until one more request fits under the per-minute limit. Every request (including retries) goes through here.
fn pace() {
    loop {
        let wait = reserve(&mut SENT.lock().unwrap(), Instant::now(), RPM_LIMIT, WINDOW);
        match wait {
            None => return,
            Some(w) => std::thread::sleep(w + Duration::from_millis(200)),
        }
    }
}

enum Fail {
    /// Try again later: the delay Google asked for, or `None` for the default backoff.
    Retry(Option<Duration>, String),
    Fatal(String),
}

fn retry_delay(v: &Value) -> Option<Duration> {
    let secs = v["error"]["details"].as_array()?.iter().find_map(|d| d["retryDelay"].as_str())?;
    secs.trim_end_matches('s').parse::<f64>().ok().map(Duration::from_secs_f64)
}

fn classify(status: u16, v: &Value) -> Fail {
    let msg = v["error"]["message"].as_str().unwrap_or("unknown error");
    match status {
        429 if v.to_string().contains("PerDay") => {
            Fail::Fatal("The free daily quota is used up. Try again tomorrow.".into())
        }
        429 => Fail::Retry(retry_delay(v), "The free per-minute limit was reached.".into()),
        500 | 502 | 503 | 504 => Fail::Retry(None, format!("Gemini is overloaded ({status}): {msg}")),
        400 | 403 if msg.contains("API key") => Fail::Fatal("The API key was rejected. Check it in Settings.".into()),
        _ => Fail::Fatal(format!("Gemini error {status}: {msg}")),
    }
}

fn call_once(key: &str, body: &Value) -> Result<String, Fail> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .into();
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{MODEL}:generateContent");
    let mut resp = agent
        .post(&url)
        .header("x-goog-api-key", key)
        .send_json(body)
        .map_err(|e| Fail::Fatal(format!("Network error: {e}")))?;
    let status = resp.status().as_u16();
    let v: Value = resp
        .body_mut()
        .read_json()
        .map_err(|e| Fail::Fatal(format!("Bad response from Gemini: {e}")))?;
    if status != 200 {
        return Err(classify(status, &v));
    }

    let cand = &v["candidates"][0];
    let text: String = cand["content"]["parts"]
        .as_array()
        .map(|p| p.iter().filter_map(|x| x["text"].as_str()).collect())
        .unwrap_or_default();
    match cand["finishReason"].as_str() {
        Some("MAX_TOKENS") => Err(Fail::Fatal("Response was cut off (too long). Try a smaller input.".into())),
        _ if text.is_empty() => {
            let why = cand["finishReason"]
                .as_str()
                .or(v["promptFeedback"]["blockReason"].as_str())
                .unwrap_or("empty response");
            Err(Fail::Fatal(format!("Gemini returned nothing ({why}).")))
        }
        _ => Ok(text),
    }
}

/// One paced request. Rate-limit and overload answers are waited out and retried instead of failing the job.
fn call(key: &str, body: Value) -> Result<String, String> {
    for attempt in 0..=MAX_RETRIES {
        pace();
        match call_once(key, &body) {
            Ok(t) => return Ok(t),
            Err(Fail::Fatal(m)) => return Err(m),
            Err(Fail::Retry(_, m)) if attempt == MAX_RETRIES => return Err(format!("{m} Try again in a minute.")),
            Err(Fail::Retry(wait, _)) => {
                let backoff = Duration::from_secs(5 << attempt); // 5, 10, 20, 40 s
                let wait = wait.map_or(backoff, |w| w + Duration::from_secs(1));
                std::thread::sleep(wait.min(Duration::from_secs(90)));
            }
        }
    }
    unreachable!("loop returns on its last attempt")
}

/// A word that mixes Cyrillic and Latin letters, like "\u{412}ona". The model rarely glitches this way
/// when writing Cyrillic. Hyphenated forms such as "USB-порт" are separate letter runs and are fine.
fn has_mixed_script(s: &str) -> bool {
    s.split(|c: char| !c.is_alphabetic()).any(|w| {
        w.chars().any(|c| ('\u{400}'..='\u{4FF}').contains(&c)) && w.chars().any(|c| c.is_ascii_alphabetic())
    })
}

fn is_cyrillic_target(to: &str) -> bool {
    matches!(to, "Ukrainian" | "Russian")
}

pub fn translate(key: &str, text: &str, dir: Dir, prev: Prev) -> Result<String, String> {
    let body = json!({
        "systemInstruction": system(dir, prev),
        "contents": [{ "parts": [{ "text": text }] }],
    });
    let out = call(key, body.clone())?;
    if is_cyrillic_target(dir.to)
        && has_mixed_script(&out)
        && let Ok(again) = call(key, body)
    {
        return Ok(again); // one retry; whatever it returns is final
    }
    Ok(out)
}

/// Translates each string independently; result has the same length and order.
pub fn translate_batch(key: &str, items: &[String], dir: Dir, prev: Prev) -> Result<Vec<String>, String> {
    let input = serde_json::to_string(items).map_err(|e| e.to_string())?;
    let body = json!({
        "systemInstruction": { "parts": [{ "text": format!(
            "{}\n\nThe input is a JSON array of strings. Return a JSON array of the same length, \
             where item i is the translation of input item i.", rules(dir, prev)) }] },
        "contents": [{ "parts": [{ "text": input }] }],
        "generationConfig": {
            "responseMimeType": "application/json",
            "responseSchema": { "type": "ARRAY", "items": { "type": "STRING" } },
        },
    });
    let attempt = || -> Result<Vec<String>, String> {
        let out = call(key, body.clone())?;
        let res: Vec<String> = serde_json::from_str(&out).map_err(|e| format!("Bad batch response: {e}"))?;
        if res.len() != items.len() {
            return Err(format!("Gemini returned {} items, expected {}.", res.len(), items.len()));
        }
        Ok(res)
    };
    let res = attempt()?;
    if is_cyrillic_target(dir.to)
        && res.iter().any(|t| has_mixed_script(t))
        && let Ok(again) = attempt()
    {
        return Ok(again);
    }
    Ok(res)
}

/// Gemini reads PDFs natively; returns the translated content as Markdown.
// ponytail: one request for the whole PDF; very long PDFs can hit the output-token cap. Split by page if needed.
pub fn translate_pdf(key: &str, pdf: &[u8], dir: Dir) -> Result<String, String> {
    if pdf.len() > MAX_PDF_BYTES {
        return Err("PDF is larger than 18 MB, which is too big to send.".into());
    }
    call(
        key,
        json!({
            "systemInstruction": system(dir, None),
            "contents": [{ "parts": [
                { "inline_data": { "mime_type": "application/pdf",
                                   "data": base64::engine::general_purpose::STANDARD.encode(pdf) } },
                { "text": format!("Translate this whole document into {}. Output Markdown that keeps headings (#), lists and tables. Put a blank line between every heading, paragraph, list and table, and keep each separate line of the original as its own paragraph.", dir.to) },
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
    fn rules_cover_direction_and_context() {
        let r = rules(Dir { from: Some("English"), to: "Ukrainian" }, None);
        assert!(r.contains("from English into Ukrainian") && r.contains("Russianisms") && !r.contains("previous_source"));
        let r = rules(Dir { from: None, to: "German" }, Some(("a", "b")));
        assert!(r.contains("Detect the source language") && r.contains("<previous_source>\na\n") && !r.contains("Russianisms"));
    }

    #[test]
    fn window_allows_limit_then_waits() {
        let (t0, win) = (Instant::now(), Duration::from_secs(60));
        let mut q = VecDeque::new();
        for i in 0..3 {
            assert!(reserve(&mut q, t0 + Duration::from_secs(i), 3, win).is_none());
        }
        // 4th at t=10s must wait until the first (t=0) leaves the window: 50s
        assert_eq!(reserve(&mut q, t0 + Duration::from_secs(10), 3, win), Some(Duration::from_secs(50)));
        assert!(reserve(&mut q, t0 + Duration::from_secs(61), 3, win).is_none()); // t=0 and t=1 have expired
        assert_eq!(q.len(), 2); // t=2 plus the new one
    }

    #[test]
    fn classifies_google_errors() {
        let minute = json!({"error": {"message": "quota", "details": [{"retryDelay": "23s"}]}});
        assert!(matches!(classify(429, &minute), Fail::Retry(Some(d), _) if d == Duration::from_secs(23)));
        let day = json!({"error": {"message": "q", "details": [{"violations": [{"quotaId": "GenerateRequestsPerDayPerProjectPerModel-FreeTier"}]}]}});
        assert!(matches!(classify(429, &day), Fail::Fatal(m) if m.contains("tomorrow")));
        assert!(matches!(classify(503, &json!({})), Fail::Retry(None, _)));
        assert!(matches!(classify(400, &json!({"error": {"message": "API key not valid"}})), Fail::Fatal(m) if m.contains("rejected")));
    }

    #[test]
    fn mixed_script_detection() {
        assert!(has_mixed_script("\u{412}ona \u{43f}\u{456}\u{448}\u{43b}\u{430}")); // Cyrillic B + Latin "ona"
        assert!(!has_mixed_script("USB-\u{43f}\u{43e}\u{440}\u{442}, Wi-Fi \u{43c}\u{435}\u{440}\u{435}\u{436}\u{430}"));
        assert!(!has_mixed_script("\u{414}\u{43e}\u{431}\u{440}\u{43e}\u{433}\u{43e} \u{440}\u{430}\u{43d}\u{43a}\u{443}, hello"));
    }

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
        let uk = Dir { from: None, to: "Ukrainian" };
        let one = translate(&key, "Good morning, how are you?", uk, None).unwrap();
        println!("single: {one}");
        let items = vec!["Hello".to_string(), "See you tomorrow & goodbye".to_string()];
        let many = translate_batch(&key, &items, Dir { from: Some("English"), to: "French" }, None).unwrap();
        println!("batch: {many:?}");
        assert_eq!(many.len(), 2);
        // rolling context: an ambiguous "bank" must follow the river sense set by the previous part
        let sentence = "She walked to the bank.";
        let alone = translate(&key, sentence, uk, None).unwrap();
        let prev = ("We sat by the river and watched the water.", "Ми сиділи біля річки й дивилися на воду.");
        let with = translate(&key, sentence, uk, Some(prev)).unwrap();
        println!("without context: {alone}\nwith context:    {with}");
    }
}
