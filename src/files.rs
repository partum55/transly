//! File translation: plain text, DOCX (paragraph text), PDF (via Gemini's native PDF input).
//! Network calls are injected as closures so the logic is testable offline.

use crate::gemini;
use quick_xml::events::{BytesRef, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

const TEXT_CHUNK: usize = 15_000; // chars per request; few requests = friendly to the free-tier daily cap
const BATCH_CHARS: usize = 8_000;
const CONTEXT_CHARS: usize = 1_200; // tail of the previous part handed to the next request
const DOC_XML: &str = "word/document.xml";

type Progress<'a> = &'a dyn Fn(String);

/// Returns every file written. `code` is the target language code used in the output file name.
pub fn run(key: &str, path: &Path, dir: gemini::Dir, code: &str, progress: Progress) -> Result<Vec<PathBuf>, String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("translated");
    let bytes = std::fs::read(path).map_err(|e| format!("Can't read file: {e}"))?;

    let mut written = Vec::new();
    let (out_ext, data) = match ext.as_str() {
        "docx" => {
            let data = translate_docx(&bytes, &mut |items, prev| gemini::translate_batch(key, items, dir, prev), progress)?;
            ("docx".to_string(), data)
        }
        "pdf" => {
            progress("Translating PDF…".into());
            let md = gemini::translate_pdf(key, &bytes, dir)?;
            // keep the translated text even if rendering fails
            let md_path = path.with_file_name(format!("{stem}.{code}.md"));
            std::fs::write(&md_path, &md).map_err(|e| format!("Can't write {}: {e}", md_path.display()))?;
            progress("Rendering PDF…".into());
            let pdf = markdown_to_pdf(&md)
                .map_err(|e| format!("Saved {} but PDF rendering failed: {e}", md_path.display()))?;
            written.push(md_path);
            ("pdf".to_string(), pdf)
        }
        _ => {
            let text = String::from_utf8(bytes).map_err(|_| "Unsupported file: need UTF-8 text, DOCX or PDF.".to_string())?;
            let out = translate_text(&text, &mut |c, prev| gemini::translate(key, c, dir, prev), progress)?;
            (if ext.is_empty() { "txt".into() } else { ext }, out.into_bytes())
        }
    };

    let out = path.with_file_name(format!("{stem}.{code}.{out_ext}"));
    std::fs::write(&out, data).map_err(|e| format!("Can't write {}: {e}", out.display()))?;
    written.insert(0, out);
    Ok(written)
}

// ponytail: markdown2pdf's "github" theme and system fonts (+ CJK fallbacks); no custom styling or bundled font.
fn markdown_to_pdf(md: &str) -> Result<Vec<u8>, String> {
    use markdown2pdf::{config::ConfigSource, fonts::{FontConfig, FontSource}};
    let mut fonts = FontConfig::new();
    for f in crate::system::cjk_fonts() {
        fonts = fonts.add_fallback_font_source(FontSource::file(f));
    }
    markdown2pdf::parse_into_bytes(md.to_string(), ConfigSource::Theme("github"), Some(&fonts)).map_err(|e| e.to_string())
}

/// Splits on line boundaries into pieces of at most `max` bytes (a single longer line stays whole).
pub fn chunk(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        if !cur.is_empty() && cur.len() + line.len() > max {
            out.push(std::mem::take(&mut cur));
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Last `CONTEXT_CHARS` characters of `s`.
fn tail(s: &str) -> &str {
    let start = s.char_indices().rev().nth(CONTEXT_CHARS - 1).map_or(0, |(i, _)| i);
    &s[start..]
}

type Prev = Option<(String, String)>; // owned (source tail, translation tail) of the previous part

fn as_prev(p: &Prev) -> gemini::Prev<'_> {
    p.as_ref().map(|(a, b)| (a.as_str(), b.as_str()))
}

/// Parts are translated in order, each one told how the part before it was translated,
/// so terms, names and tone stay consistent across a long text.
pub fn translate_text(
    text: &str,
    tr: &mut dyn FnMut(&str, gemini::Prev) -> Result<String, String>,
    progress: Progress,
) -> Result<String, String> {
    if text.trim().is_empty() {
        return Err("File is empty.".into());
    }
    let chunks = chunk(text, TEXT_CHUNK);
    let mut out = String::new();
    let mut prev: Prev = None;
    for (i, c) in chunks.iter().enumerate() {
        progress(format!("Translating part {}/{}…", i + 1, chunks.len()));
        if c.trim().is_empty() {
            out.push_str(c);
            continue;
        }
        let t = tr(c, as_prev(&prev))?;
        prev = Some((tail(c).to_string(), tail(&t).to_string()));
        // models tend to strip trailing newlines; restore what the source had
        let trailing = c.len() - c.trim_end_matches('\n').len();
        out.push_str(t.trim_end_matches('\n'));
        out.push_str(&"\n".repeat(trailing));
    }
    Ok(out)
}

// ---------- DOCX ----------

type BatchFn<'a> = &'a mut dyn FnMut(&[String], gemini::Prev) -> Result<Vec<String>, String>;

// ponytail: only word/document.xml (body, tables); headers/footers/footnotes are left untranslated.
// Inline formatting inside a paragraph is flattened to the paragraph's first run.
pub fn translate_docx(docx: &[u8], tr: BatchFn, progress: Progress) -> Result<Vec<u8>, String> {
    let mut zin = zip::ZipArchive::new(Cursor::new(docx)).map_err(|_| "Not a valid DOCX file.".to_string())?;
    let mut xml = Vec::new();
    zin.by_name(DOC_XML)
        .map_err(|_| "DOCX has no word/document.xml.".to_string())?
        .read_to_end(&mut xml)
        .map_err(|e| e.to_string())?;

    let paras = paragraph_texts(&xml)?;
    let todo: Vec<usize> = (0..paras.len()).filter(|&i| !paras[i].trim().is_empty()).collect();
    if todo.is_empty() {
        return Err("No text found in the document.".into());
    }

    // group paragraphs into batches by size
    let mut batches: Vec<Vec<usize>> = vec![vec![]];
    let mut size = 0;
    for &i in &todo {
        if size > 0 && size + paras[i].len() > BATCH_CHARS {
            batches.push(vec![]);
            size = 0;
        }
        batches.last_mut().unwrap().push(i);
        size += paras[i].len();
    }

    let mut translated: Vec<Option<String>> = vec![None; paras.len()];
    let mut prev: Prev = None;
    for (n, b) in batches.iter().enumerate() {
        progress(format!("Translating part {}/{}…", n + 1, batches.len()));
        let input: Vec<String> = b.iter().map(|&i| paras[i].clone()).collect();
        let out = tr(&input, as_prev(&prev))?;
        prev = Some((tail(&input.join("\n")).to_string(), tail(&out.join("\n")).to_string()));
        for (&i, t) in b.iter().zip(out) {
            translated[i] = Some(t);
        }
    }

    let new_xml = rewrite(&xml, &translated)?;

    let mut out = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for i in 0..zin.len() {
        let f = zin.by_index(i).map_err(|e| e.to_string())?;
        if f.name_raw() == DOC_XML.as_bytes() {
            out.start_file(DOC_XML, zip::write::SimpleFileOptions::default()).map_err(|e| e.to_string())?;
            out.write_all(&new_xml).map_err(|e| e.to_string())?;
        } else {
            out.raw_copy_file(f).map_err(|e| e.to_string())?;
        }
    }
    Ok(out.finish().map_err(|e| e.to_string())?.into_inner())
}

fn push_ref(s: &mut String, g: &BytesRef) {
    if let Ok(Some(c)) = g.resolve_char_ref() {
        s.push(c);
        return;
    }
    s.push(match &**g {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => return,
    });
}

/// Text of every `<w:p>` in document order (including empty ones), so indices match `rewrite`.
fn paragraph_texts(xml: &[u8]) -> Result<Vec<String>, String> {
    let mut r = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut paras: Vec<String> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut in_t = false;
    loop {
        buf.clear();
        match r.read_event_into(&mut buf).map_err(|e| e.to_string())? {
            Event::Eof => break,
            Event::Start(e) if e.name().as_ref() == "w:p" => {
                paras.push(String::new());
                stack.push(paras.len() - 1);
            }
            Event::End(e) if e.name().as_ref() == "w:p" => {
                stack.pop();
            }
            Event::Start(e) if e.name().as_ref() == "w:t" => in_t = true,
            Event::End(e) if e.name().as_ref() == "w:t" => in_t = false,
            Event::Text(t) if in_t => {
                if let Some(&i) = stack.last() {
                    paras[i].push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(g) if in_t => {
                if let Some(&i) = stack.last() {
                    push_ref(&mut paras[i], &g);
                }
            }
            _ => {}
        }
    }
    Ok(paras)
}

/// Puts each translation into its paragraph's first `<w:t>` and empties the paragraph's other `<w:t>`s.
fn rewrite(xml: &[u8], tr: &[Option<String>]) -> Result<Vec<u8>, String> {
    let e2s = |e: quick_xml::Error| e.to_string();
    let mut r = Reader::from_reader(xml);
    let mut w = Writer::new(Vec::new());
    let mut buf = Vec::new();
    let mut next = 0;
    let mut stack: Vec<(usize, bool)> = Vec::new(); // (paragraph index, first run written)
    let mut skipping = false; // inside a <w:t> whose text we replace
    loop {
        buf.clear();
        let ev = r.read_event_into(&mut buf).map_err(e2s)?;
        match &ev {
            Event::Eof => break,
            Event::Start(e) if e.name().as_ref() == "w:p" => {
                stack.push((next, false));
                next += 1;
            }
            Event::End(e) if e.name().as_ref() == "w:p" => {
                stack.pop();
            }
            Event::Start(e) if e.name().as_ref() == "w:t" => {
                if let Some((i, wrote)) = stack.last_mut()
                    && let Some(t) = &tr[*i]
                {
                    skipping = true;
                    if !*wrote {
                        *wrote = true;
                        let mut s = BytesStart::new("w:t");
                        s.push_attribute(("xml:space", "preserve"));
                        w.write_event(Event::Start(s)).map_err(|e| e.to_string())?;
                        w.write_event(Event::Text(BytesText::new(t))).map_err(|e| e.to_string())?;
                        continue;
                    }
                }
            }
            Event::End(e) if e.name().as_ref() == "w:t" => skipping = false,
            Event::Text(_) | Event::GeneralRef(_) if skipping => continue,
            _ => {}
        }
        w.write_event(ev).map_err(|e| e.to_string())?;
    }
    Ok(w.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="w"><w:body>
<w:p><w:r><w:t>Hello </w:t></w:r><w:r><w:t>Tom &amp; Jerry</w:t></w:r></w:p>
<w:p/><w:p><w:r><w:t>Second</w:t></w:r></w:p></w:body></w:document>"#;

    fn make_docx() -> Vec<u8> {
        let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let o = zip::write::SimpleFileOptions::default();
        z.start_file("[Content_Types].xml", o).unwrap();
        z.write_all(b"<Types/>").unwrap();
        z.start_file(DOC_XML, o).unwrap();
        z.write_all(DOC.as_bytes()).unwrap();
        z.finish().unwrap().into_inner()
    }

    #[test]
    fn docx_roundtrip() {
        let mut seen = vec![];
        let out = translate_docx(
            &make_docx(),
            &mut |items, _| {
                seen.extend_from_slice(items);
                Ok(items.iter().map(|s| s.to_uppercase() + " <&>").collect())
            },
            &|_| {},
        )
        .unwrap();
        assert_eq!(seen, ["Hello Tom & Jerry", "Second"]);

        let mut z = zip::ZipArchive::new(Cursor::new(out)).unwrap();
        assert!(z.by_name("[Content_Types].xml").is_ok());
        let mut xml = Vec::new();
        z.by_name(DOC_XML).unwrap().read_to_end(&mut xml).unwrap();
        let paras = paragraph_texts(&xml).unwrap();
        assert_eq!(paras, ["HELLO TOM & JERRY <&>", "SECOND <&>"]); // 2nd run of para 1 emptied
        assert!(String::from_utf8(xml).unwrap().contains("<w:p/>"));
    }

    #[test]
    fn markdown_renders_to_pdf() {
        let pdf = markdown_to_pdf("# Titel\n\nÜber **fett** – Привет 你好\n\n| a | b |\n|---|---|\n| 1 | 2 |\n").unwrap();
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn chunks_keep_order_and_lines() {
        let text = "aaa\nbbb\n\nccc\nddd";
        let c = chunk(text, 8);
        assert_eq!(c.concat(), text);
        assert!(c.iter().all(|p| p.len() <= 8 && (p.ends_with('\n') || p == c.last().unwrap())));
    }

    #[test]
    fn later_parts_get_the_previous_part_as_context() {
        let text = format!("{}\n{}\n", "a".repeat(10_000), "b".repeat(10_000)); // two chunks
        let mut seen: Vec<Option<(usize, usize)>> = vec![];
        let out = translate_text(
            &text,
            &mut |c, prev| {
                seen.push(prev.map(|(s, t)| (s.chars().filter(|&x| x == 'a').count(), t.chars().filter(|&x| x == 'A').count())));
                Ok(c.to_uppercase())
            },
            &|_| {},
        )
        .unwrap();
        // the tail is the last CONTEXT_CHARS chars of the chunk, which ends with a newline
        assert_eq!(seen, [None, Some((CONTEXT_CHARS - 1, CONTEXT_CHARS - 1))]);
        assert_eq!(out.len(), text.len());
    }

    #[test]
    fn tail_is_char_safe() {
        let s = "я".repeat(2_000); // 2 bytes per char
        assert_eq!(tail(&s).chars().count(), CONTEXT_CHARS);
        assert_eq!(tail("short"), "short");
    }

    #[test]
    fn text_keeps_trailing_newlines() {
        let out = translate_text("a\n\nb\n", &mut |c, _| Ok(c.to_uppercase().trim_end().to_string()), &|_| {}).unwrap();
        assert_eq!(out, "A\n\nB\n");
    }
}
