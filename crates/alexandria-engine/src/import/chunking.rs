/// Document chunking strategies for import.

/// A single chunk with its content and optional heading context.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub content: String,
    pub index: usize,
    pub heading: Option<String>,
}

/// Split a document by markdown headings (`#`, `##`, etc.).
/// Each section becomes a chunk, with the heading preserved.
pub fn chunk_by_heading(text: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut current_lines: Vec<&str> = Vec::new();

    for line in text.lines() {
        if line.starts_with('#') {
            // Flush current section
            if !current_lines.is_empty() {
                let content = current_lines.join("\n").trim().to_string();
                if !content.is_empty() {
                    chunks.push(Chunk {
                        content,
                        index: chunks.len(),
                        heading: current_heading.clone(),
                    });
                }
            }
            current_heading = Some(line.trim_start_matches('#').trim().to_string());
            current_lines.clear();
            current_lines.push(line);
        } else {
            current_lines.push(line);
        }
    }

    // Flush last section
    if !current_lines.is_empty() {
        let content = current_lines.join("\n").trim().to_string();
        if !content.is_empty() {
            chunks.push(Chunk {
                content,
                index: chunks.len(),
                heading: current_heading,
            });
        }
    }

    chunks
}

/// Split a document by paragraphs (double newline separated).
pub fn chunk_by_paragraph(text: &str) -> Vec<Chunk> {
    text.split("\n\n")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .enumerate()
        .map(|(i, content)| Chunk {
            content: content.to_string(),
            index: i,
            heading: None,
        })
        .collect()
}

/// Split a document into fixed-size chunks with overlap.
pub fn chunk_by_fixed_size(text: &str, chunk_size: usize, overlap: usize) -> Vec<Chunk> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }

    let step = chunk_size.saturating_sub(overlap).max(1);
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < chars.len() {
        let end = (start + chunk_size).min(chars.len());
        let content: String = chars[start..end].iter().collect();
        let content = content.trim().to_string();
        if !content.is_empty() {
            chunks.push(Chunk {
                content,
                index: chunks.len(),
                heading: None,
            });
        }
        start += step;
        if end == chars.len() {
            break;
        }
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_by_heading() {
        let doc = "# Introduction\nSome intro text\n\n# Methods\nMethod details\n\n# Results\nResults here";
        let chunks = chunk_by_heading(doc);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].heading.as_deref(), Some("Introduction"));
        assert!(chunks[0].content.contains("intro text"));
        assert_eq!(chunks[1].heading.as_deref(), Some("Methods"));
        assert_eq!(chunks[2].heading.as_deref(), Some("Results"));
    }

    #[test]
    fn test_chunk_by_paragraph() {
        let doc = "First paragraph.\n\nSecond paragraph.\n\nThird paragraph.";
        let chunks = chunk_by_paragraph(doc);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].content, "First paragraph.");
        assert_eq!(chunks[1].content, "Second paragraph.");
    }

    #[test]
    fn test_chunk_by_fixed_size() {
        let doc = "abcdefghijklmnopqrstuvwxyz";
        let chunks = chunk_by_fixed_size(doc, 10, 2);
        assert!(!chunks.is_empty());
        assert!(chunks[0].content.len() <= 10);
    }

    #[test]
    fn test_empty_document() {
        assert!(chunk_by_heading("").is_empty());
        assert!(chunk_by_paragraph("").is_empty());
        assert!(chunk_by_fixed_size("", 10, 2).is_empty());
    }
}
