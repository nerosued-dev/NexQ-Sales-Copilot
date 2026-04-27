use std::path::Path;

/// Extract text content from a PDF file.
/// Tries pdf_extract first; if it returns empty, falls back to lopdf
/// which handles Type1/CID fonts with custom encoding (common in book PDFs).
pub fn extract_text_from_pdf(file_path: &str) -> Result<String, String> {
    let path = Path::new(file_path);

    if !path.exists() {
        return Err(format!("PDF file not found: {}", file_path));
    }

    // Primary: pdf_extract
    match pdf_extract::extract_text(path) {
        Ok(text) => {
            let trimmed = text.trim().to_string();
            if !trimmed.is_empty() {
                return Ok(trimmed);
            }
            log::warn!("pdf_extract returned empty for '{}', trying lopdf fallback", file_path);
        }
        Err(e) => {
            log::warn!("pdf_extract failed for '{}': {} — trying lopdf fallback", file_path, e);
        }
    }

    // Fallback: lopdf — parses content streams directly, handles more encoding variants
    extract_with_lopdf(file_path).and_then(|text| {
        if text.trim().is_empty() {
            Err(
                "PDF has no extractable text. It may be a scanned/image-based PDF \
                 (no text layer) or use an unsupported font encoding. \
                 Try exporting as a text-based PDF or convert to .txt first."
                    .to_string(),
            )
        } else {
            Ok(text)
        }
    })
}

fn extract_with_lopdf(file_path: &str) -> Result<String, String> {
    use lopdf::content::Content;
    use lopdf::Document;

    let doc = Document::load(file_path)
        .map_err(|e| format!("lopdf failed to load PDF: {}", e))?;

    let mut full_text = String::new();

    for page_id in doc.page_iter() {
        let content_bytes = match doc.get_page_content(page_id) {
            Ok(b) => b,
            Err(_) => continue,
        };

        let content = match Content::decode(&content_bytes) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let mut in_text_block = false;
        let mut page_text = String::new();

        for op in &content.operations {
            match op.operator.as_str() {
                "BT" => {
                    in_text_block = true;
                }
                "ET" => {
                    in_text_block = false;
                    page_text.push('\n');
                }
                "Tj" if in_text_block => {
                    if let Some(lopdf::Object::String(bytes, _)) = op.operands.first() {
                        page_text.push_str(&decode_pdf_string(bytes));
                    }
                }
                "TJ" if in_text_block => {
                    if let Some(lopdf::Object::Array(arr)) = op.operands.first() {
                        for item in arr {
                            if let lopdf::Object::String(bytes, _) = item {
                                page_text.push_str(&decode_pdf_string(bytes));
                            }
                            // negative kern values between glyphs — ignore numbers
                        }
                    }
                }
                // Text positioning operators that imply a word/line break
                "Td" | "TD" | "T*" => {
                    if in_text_block && !page_text.ends_with('\n') {
                        page_text.push(' ');
                    }
                }
                "Tm" => {
                    if in_text_block && !page_text.ends_with('\n') {
                        page_text.push('\n');
                    }
                }
                _ => {}
            }
        }

        full_text.push_str(&page_text);
        full_text.push('\n');
    }

    Ok(full_text.trim().to_string())
}

/// Decode a PDF byte string to UTF-8.
/// Handles UTF-16BE (with BOM), then falls back to Windows-1252 (covers Latin-1 / Spanish).
fn decode_pdf_string(bytes: &[u8]) -> String {
    // UTF-16BE with BOM (FE FF)
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let utf16: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|c| u16::from_be_bytes([c[0], c.get(1).copied().unwrap_or(0)]))
            .collect();
        if let Ok(s) = String::from_utf16(&utf16) {
            return s;
        }
    }

    // Try UTF-8 first (some PDFs embed UTF-8 strings)
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }

    // Fallback: Windows-1252 (superset of Latin-1, handles all Spanish characters)
    let (decoded, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
    decoded.into_owned()
}
