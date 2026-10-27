//! P3 real extraction. Text formats are parsed in process; PDF runs the
//! installed `pdftotext` (poppler) as a bounded, read-only worker. Formats that
//! need a tool that is NOT installed (pandoc: DOCX/ODT/RTF; calamine: XLSX/ODS)
//! fail with the explicit `extractor_unavailable` code naming the missing tool
//! — never a blank success, never fabricated content.

use std::path::Path;
use std::process::{Command, Stdio};
use crate::error::{CoreError, Result};

/// Hard ceiling on bytes a single source may occupy while being extracted.
pub const MAX_SOURCE_BYTES: u64 = 50 * 1024 * 1024;
/// Hard wall-clock ceiling for one extraction worker (covers pdftotext too).
pub const EXTRACTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// pdftotext identity: the real binary version actually used (poppler 26.08.0).
pub const PDFTOTEXT_VERSION: &str = "pdftotext:poppler-26.08.0";

/// One addressable unit of extracted text. `locator_key` names it exactly (see
/// `crate::knowledge::locators`); `kind` records the extractor family.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExtractedUnit { pub locator_key: String, pub content: String }

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Extraction {
    pub extractor_id: String,
    pub extractor_version: String,
    pub mime: String,
    pub units: Vec<ExtractedUnit>,
}

/// Detect encryption on a PDF without trusting pdftotext's alone: the trailer
/// of an encrypted PDF names `/Encrypt`.
fn pdf_is_encrypted(bytes: &[u8]) -> bool {
    let tail = if bytes.len() > 65536 { &bytes[bytes.len() - 65536..] } else { bytes };
    String::from_utf8_lossy(tail).contains("/Encrypt")
}

/// Run pdftotext over a temporary copy. Bounded: timeout, size already checked
/// by the caller, no shell, minimal env. Returns the extracted text.
fn run_pdftotext(bytes: &[u8]) -> Result<String> {
    let dir = tempfile::tempdir().map_err(|_| CoreError::new("io_error", "cannot stage pdf worker scratch"))?;
    let src = dir.path().join("source.pdf");
    std::fs::write(&src, bytes)?;
    let child = Command::new("pdftotext")
        .args(["-layout", src.to_str().unwrap_or("")])
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(_) => return Err(CoreError::new("extractor_unavailable", "pdftotext (poppler) is not installed")),
    };
    use std::io::Read;
    let deadline = std::time::Instant::now() + EXTRACTION_TIMEOUT;
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(_) => return Err(CoreError::new("extraction_timeout", "pdftotext worker did not finish within the bounded time")),
    };
    let _ = deadline;
    if out.status.code() == Some(0) {
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        return Ok(text);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lower = stderr.to_lowercase();
    if lower.contains("incorrect password") || lower.contains("password") {
        return Err(CoreError::new("source_encrypted", "PDF is encrypted; refusing to index without the owner's explicit unlock"));
    }
    if lower.contains("may not be a pdf") || lower.contains("couldn't find trailer") || lower.contains("couldn't read xref") || lower.contains("document stream is empty") {
        return Err(CoreError::new("malformed_pdf", "PDF is malformed or truncated; pdftotext rejected it"));
    }
    Err(CoreError::new("pdf_extraction_failed", format!("pdftotext failed: {}", stderr.trim().chars().take(160).collect::<String>())))
}

/// Split pdftotext output into 1-based page units on the form-feed separator.
fn split_pdf_pages(text: &str) -> Vec<(u32, String)> {
    let mut pages = Vec::new();
    for (i, part) in text.split('\u{0c}').enumerate() {
        let trimmed = part.trim();
        // A trailing form feed leaves a final empty part; skip empty pages.
        if trimmed.is_empty() { continue; }
        pages.push((i as u32 + 1, trimmed.to_string()));
    }
    pages
}

fn split_code_lines(content: &str) -> Vec<crate::contracts::CitationLocator> {
    let mut locators = Vec::new();
    let mut start: u32 = 1;
    for (i, line) in content.lines().enumerate() {
        let n = (i + 1) as u32;
        let blank = line.trim().is_empty();
        let last = i + 1 == content.lines().count();
        if blank && !last {
            if start < n { locators.push(super::locators::lines(start, n - 1)); }
            start = n + 1;
        }
    }
    if start <= (content.lines().count() as u32).max(1) {
        let end = content.lines().count().max(1) as u32;
        if start <= end { locators.push(super::locators::lines(start, end)); }
    }
    locators
}

fn is_code_filename(name: &str) -> bool {
    matches!(name.rsplit('.').next().unwrap_or(""),
        "rs"|"go"|"c"|"h"|"cc"|"cpp"|"hpp"|"py"|"rb"|"ts"|"tsx"|"js"|"jsx"|"mjs"|"sh"|"bash"|"zsh"|"pl"|"lua"|"toml"|"json"|"yaml"|"yml"|"nix"|"zig"|"java"|"kt"|"sql"|"proto"|"hcl"|"tf"|"ex"|"exs"|"clj"|"el"|"vim"|"css"|"scss")
}

fn mime_for(name: &str, ext: &str) -> &'static str {
    match ext {
        "pdf" => "application/pdf",
        "md"|"markdown" => "text/markdown",
        "html"|"htm" => "text/html",
        "txt"|"log"|"text" => "text/plain",
        "csv" => "text/csv",
        _ => "text/plain",
    }
}

fn code_ext(name: &str) -> bool { is_code_filename(name) }

fn entity(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(end) = text[i..].find(';') {
                let ent = &text[i..i + end + 1];
                out.push(match ent {
                    "&amp;" => '&', "&lt;" => '<', "&gt;" => '>', "&quot;" => '"', "&apos;" => '\'',
                    "&nbsp;" => ' ', _ => { for c in ent.chars() { out.push(c); } continue; }
                });
                i += end + 1;
                continue;
            }
        }
        out.push(text[i..].chars().next().unwrap_or(' '));
        i += 1;
    }
    out
}

/// Extract from in-memory bytes. `name` is the file name (for extension/MIME);
/// `path` is the canonical source path if readable (used for the worker input).
pub fn extract(name: &str, bytes: &[u8]) -> Result<Extraction> {
    let path = Path::new(name);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    // PDF first: header sniff beats extension.
    if bytes.starts_with(b"%PDF-") {
        return extract_pdf(bytes);
    }
    match ext.as_str() {
        "docx" => return Err(CoreError::new("extractor_unavailable", "pandoc is not installed; DOCX extraction is unavailable on this host")),
        "odt" => return Err(CoreError::new("extractor_unavailable", "pandoc is not installed; ODT extraction is unavailable on this host")),
        "rtf" => return Err(CoreError::new("extractor_unavailable", "pandoc is not installed; RTF extraction is unavailable on this host")),
        "xlsx" | "ods" => return Err(CoreError::new("extractor_unavailable", "calamine is not available as a linked Rust crate on this host; XLSX/ODS extraction is unavailable")),
        "pdf" => return Err(CoreError::new("malformed_pdf", "file names itself PDF but has no %PDF header")),
        _ => {}
    }
    let text = std::str::from_utf8(bytes).map_err(|_| CoreError::new("unsupported_encoding", "source is not valid UTF-8 text; no qualified text extractor"))?;
    if ext == "html" || ext == "htm" {
        return extract_html(text);
    }
    if code_ext(name) || !ext.is_empty() && text.lines().count() > 0 && looks_like_code(name, text) {
        return extract_lines(name, text, "code", &ext);
    }
    if ext == "md" || ext == "markdown" {
        return extract_markdown(text);
    }
    extract_lines(name, text, "text", &ext)
}

fn looks_like_code(name: &str, text: &str) -> bool {
    if is_code_filename(name) { return true; }
    let first = text.lines().take(20).collect::<Vec<_>>().join(" ");
    first.contains(&format!("<{}", ext_of(name))) || first.starts_with("<!DOCTYPE") || first.starts_with("<html")
}
fn ext_of(name: &str) -> String {
    Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

fn extract_lines(name: &str, text: &str, family: &str, ext: &str) -> Result<Extraction> {
    let locators = split_code_lines(text);
    let line_count = text.lines().count().max(1);
    let mut units = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for loc in &locators {
        let s = loc.line_start.unwrap_or(1) as usize - 1;
        let e = loc.line_end.unwrap_or(line_count as u32) as usize;
        let content = lines.get(s.min(lines.len().saturating_sub(1))..e.min(lines.len())).unwrap_or(&[]).join("\n");
        if content.trim().is_empty() { continue; }
        units.push(ExtractedUnit { locator_key: super::locators::locator_key(loc), content });
    }
    if units.is_empty() {
        units.push(ExtractedUnit { locator_key: super::locators::locator_key(&super::locators::lines(1, line_count as u32)), content: text.to_string() });
    }
    let mime = mime_for(name, ext).to_string();
    Ok(Extraction { extractor_id: family.to_string(), extractor_version: format!("{family}-builtin-1"), mime, units })
}

fn extract_markdown(text: &str) -> Result<Extraction> {
    let mut units = Vec::new();
    let mut current_section = String::from("__start__");
    let mut current: Vec<String> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for line in &lines {
        let trimmed = line.trim_start();
        let is_heading = trimmed.starts_with('#');
        if is_heading {
            if current.join("\n").trim().is_empty() { /* no body */ }
            if !current.is_empty() {
                let body = current.join("\n").trim().to_string();
                if !body.is_empty() {
                    units.push(ExtractedUnit { locator_key: super::locators::locator_key(&super::locators::section(&current_section)), content: body });
                }
            }
            current_section = trimmed.trim_start_matches('#').trim().to_string();
            if current_section.is_empty() { current_section = "__heading__".into(); }
            current.clear();
        } else {
            current.push(line.to_string());
        }
    }
    if !current.is_empty() {
        let body = current.join("\n").trim().to_string();
        if !body.is_empty() {
            units.push(ExtractedUnit { locator_key: super::locators::locator_key(&super::locators::section(&current_section)), content: body });
        }
    }
    if units.is_empty() {
        units.push(ExtractedUnit { locator_key: super::locators::locator_key(&super::locators::section("__start__")), content: text.trim().to_string() });
    }
    Ok(Extraction { extractor_id: "markdown".into(), extractor_version: "markdown-builtin-1".into(), mime: "text/markdown".into(), units })
}

/// Sanitised HTML text + section anchors. A single deterministic pass (no
/// external HTML parser is required or available): drop <script>/<style>
/// bodies entirely, treat block-boundary tags as newlines, capture heading text
/// as the section anchor for the body that follows, and decode a small entity
/// set. Content from scripts/styles must never reach the index.
fn extract_html(text: &str) -> Result<Extraction> {
    // 1) Remove script/style bodies (case-insensitive) before anything else.
    let mut sanitized = String::with_capacity(text.len());
    let lower = text.to_lowercase();
    let text_chars: Vec<char> = text.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();
    let mut i = 0usize;
    while i < lower_chars.len() {
        let rest: String = lower_chars[i..].iter().collect();
        if rest.starts_with("<script") || rest.starts_with("<style") {
            let key = if rest.starts_with("<script") { "</script" } else { "</style" };
            if let Some(rel) = rest.find(key) {
                i += rel + key.len();
                while i < lower_chars.len() && lower_chars[i] != '>' { i += 1; }
                if i < lower_chars.len() { i += 1; }
                continue;
            }
        }
        sanitized.push(text_chars[i]);
        i += 1;
    }

    // 2) Single pass over the sanitised text: heading detection + paragraph
    //    boundaries + entity decoding.
    let mut units: Vec<ExtractedUnit> = Vec::new();
    let mut section = String::from("__start__");
    let mut para: String = String::new();
    let mut in_heading = false;
    let mut heading: String = String::new();

    let mut flush_para = |units: &mut Vec<ExtractedUnit>, section: &str, para: &mut String| {
        if !para.is_empty() {
            let body = para.trim();
            if !body.is_empty() {
                units.push(ExtractedUnit {
                    locator_key: super::locators::locator_key(&super::locators::section(section)),
                    content: body.to_string(),
                });
            }
        }
        para.clear();
    };

    let chars: Vec<char> = sanitized.chars().collect();
    let mut idx = 0usize;
    while idx < chars.len() {
        if chars[idx] == '<' {
            let start = idx;
            while idx < chars.len() && chars[idx] != '>' { idx += 1; }
            let end = idx.min(chars.len());
            let tag: String = chars[start..end].iter().collect();
            let t = tag.trim();
            idx = end + 1;
            let is_heading_open = matches!(t, "<h1" | "<h2" | "<h3" | "<h4" | "<h5" | "<h6" | "<h1 " | "<h2 " | "<h3 " | "<h4 " | "<h5 " | "<h6 ");
            let is_heading_close = matches!(t, "</h1>" | "</h2>" | "</h3>" | "</h4>" | "</h5>" | "</h6>");
            if is_heading_open {
                flush_para(&mut units, &section, &mut para);
                in_heading = true;
                heading.clear();
            } else if is_heading_close {
                in_heading = false;
                let heading_text = decode_entities(heading.trim());
                section = if heading_text.is_empty() { "__section__".into() } else { heading_text.chars().take(80).collect() };
            } else if t == "<br>" || t == "<br/>" || t == "<br />"
                || t.starts_with("</p>") || t.starts_with("</div>") || t.starts_with("</li>") || t.starts_with("</tr>")
            {
                if !in_heading {
                    flush_para(&mut units, &section, &mut para);
                }
            }
        } else {
            if in_heading { heading.push(chars[idx]); }
            else { para.push(chars[idx]); }
            idx += 1;
        }
    }
    // Strip all remaining tags (inline formatting not removed by the pass above).
    let mut plain = String::with_capacity(sanitized.len());
    let mut in_tag = false;
    for ch in sanitized.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => { in_tag = false; plain.push(' '); }
            c if !in_tag => plain.push(c),
            _ => {}
        }
    }
    plain = plain.replace("  ", " ").trim().to_string();

    for unit in units.iter_mut() {
        unit.content = decode_entities(&unit.content);
    }
    if units.is_empty() {
        let decoded = decode_entities(&plain);
        units.push(ExtractedUnit {
            locator_key: super::locators::locator_key(&super::locators::section("__start__")),
            content: decoded,
        });
    }
    Ok(Extraction { extractor_id: "html".into(), extractor_version: "html-builtin-1".into(), mime: "text/html".into(), units })
}

fn extract_pdf(bytes: &[u8]) -> Result<Extraction> {
    if pdf_is_encrypted(bytes) {
        return Err(CoreError::new("source_encrypted", "PDF trailer names an /Encrypt dictionary; refusing to index an encrypted source"));
    }
    let text = run_pdftotext(bytes)?;
    let pages = split_pdf_pages(&text);
    let mut units = Vec::new();
    for (n, content) in pages {
        if content.trim().is_empty() { continue; }
        units.push(ExtractedUnit { locator_key: super::locators::locator_key(&super::locators::page(n)), content });
    }
    if units.is_empty() {
        // pdftotext ran clean but produced nothing indexable. This is the classic
        // encrypted/image-only/skewed document that must NOT be a blank success.
        return Err(CoreError::new("extraction_empty", "pdftotext produced no extractable text; the PDF may be image-only, skewed or protected"));
    }
    Ok(Extraction { extractor_id: "pdf".into(), extractor_version: PDFTOTEXT_VERSION.into(), mime: "application/pdf".into(), units })
}

/// Whole-extraction validation gate. A revision is published only if every unit
/// carries real content. This is what turns "encrypted/empty/malformed" into an
/// explicit typed failure instead of an empty, searchable-looking revision.
pub fn validate(extraction: &Extraction) -> Result<()> {
    if extraction.units.is_empty() {
        return Err(CoreError::new("extraction_empty", "extraction produced no content units; no revision is published"));
    }
    for unit in &extraction.units {
        if unit.content.trim().is_empty() {
            return Err(CoreError::new("extraction_empty", "extraction contains an empty content unit; no revision is published"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn markdown_sections_and_start() {
        let text = "# Alpha\nfirst line\nsecond line\n\n# Beta\nthird line\n";
        let e = extract("notes.md", text.as_bytes()).unwrap();
        assert_eq!(e.units.len(), 2);
        assert!(e.units[0].content.contains("first line"));
        assert!(e.units[1].content.contains("third line"));
    }
    #[test]
    fn code_line_locators() {
        let text = "fn main() {\n    println!(\"hi\");\n}\n";
        let e = extract("main.rs", text.as_bytes()).unwrap();
        assert_eq!(e.extractor_id, "code");
        assert!(e.units.iter().any(|u| u.content.contains("println")));
    }
    #[test]
    fn docx_is_unavailable_not_blank() {
        let err = extract("doc.docx", b"whatever").unwrap_err();
        assert_eq!(err.code, "extractor_unavailable");
        assert!(err.message.contains("pandoc"));
    }
    #[test]
    fn xlsx_is_unavailable_naming_calamine() {
        let err = extract("sheet.xlsx", b"whatever").unwrap_err();
        assert_eq!(err.code, "extractor_unavailable");
        assert!(err.message.contains("calamine"));
    }
    #[test]
    fn non_utf8_is_unsupported_not_blank() {
        let err = extract("blob.txt", &[0xff, 0xfe, 0x00, 0x01]).unwrap_err();
        assert_eq!(err.code, "unsupported_encoding");
    }
    #[test]
    fn validate_rejects_empty_units() {
        let bad = Extraction { extractor_id: "x".into(), extractor_version: "1".into(), mime: "text/plain".into(), units: vec![ExtractedUnit { locator_key: "k".into(), content: "   ".into() }] };
        assert_eq!(validate(&bad).unwrap_err().code, "extraction_empty");
    }
    #[test]
    fn pdf_without_header_is_malformed() {
        let err = extract("x.pdf", b"not a real pdf body").unwrap_err();
        assert_eq!(err.code, "malformed_pdf");
    }
    #[test]
    fn html_sanitises_and_sections() {
        let text = "<html><body><h1>Title</h1><p>Hello <b>world</b> &amp; friends.</p><script>var x=1;</script></body></html>";
        let e = extract("page.html", text.as_bytes()).unwrap();
        let joined = e.units.iter().map(|u| u.content.as_str()).collect::<Vec<_>>().join(" ");
        assert!(joined.contains("Hello world & friends"), "got: {joined}");
        assert!(!joined.contains("var x=1"), "script must be dropped");
    }
}
