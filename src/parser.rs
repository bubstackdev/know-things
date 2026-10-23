//! Source offsets always address original UTF-8 bytes. No newline normalization.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub start: usize,
    pub end: usize,
    pub heading: String,
    pub continuation: bool,
}
pub const PROCESSING: &str = "markdown-blocks-v3-max4096bytes-heading4096";
pub const BUDGET_TOKENIZER: &str = "utf8-byte-v1";
// A named, conservative byte tokenizer for retrieval-only budgeting. This is
// deliberately not presented as a generation or embedding model's token count.
pub fn token_count(text: &str) -> usize {
    text.len()
}

pub fn parse(text: &str, markdown: bool) -> crate::contracts::Result<Vec<Chunk>> {
    let mut chunks = Vec::new();
    let mut headings: Vec<String> = Vec::new();
    let mut start = 0;
    let mut offset = 0;
    let mut fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        let structural =
            line.chars().take_while(|c| *c == ' ').count() <= 3 && !line.starts_with('\t');
        let marker = if structural && trimmed.starts_with("```") {
            Some('`')
        } else if structural && trimmed.starts_with("~~~") {
            Some('~')
        } else {
            None
        };
        if markdown && structural && fence.is_none() && trimmed.starts_with('#') {
            let depth = trimmed.chars().take_while(|c| *c == '#').count();
            if depth <= 6
                && trimmed
                    .as_bytes()
                    .get(depth)
                    .is_some_and(u8::is_ascii_whitespace)
            {
                push(&mut chunks, text, start, offset, &headings, false);
                headings.truncate(depth - 1);
                while headings.len() < depth - 1 {
                    headings.push(String::new());
                }
                let title = trimmed[depth..].trim();
                if title.len() > 1024
                    || headings.iter().map(String::len).sum::<usize>() + title.len() + depth * 3
                        > 4096
                {
                    return Err(crate::contracts::Error::new(
                        "resource_exhausted",
                        "Heading metadata exceeds the parser limit",
                    ));
                }
                headings.push(title.to_owned());
                start = offset;
            }
        }
        if markdown && let Some(c) = marker {
            let n = trimmed.chars().take_while(|x| *x == c).count();
            match fence {
                None => {
                    fence = Some((c, n));
                }
                Some((f, m)) if c == f && n >= m && trimmed[n..].trim().is_empty() => {
                    fence = None;
                }
                _ => {}
            }
        }
        offset += line.len();
        if trimmed.is_empty() && fence.is_none() {
            push(&mut chunks, text, start, offset, &headings, false);
            start = offset;
        }
    }
    push(&mut chunks, text, start, text.len(), &headings, false);
    Ok(chunks)
}
fn push(
    out: &mut Vec<Chunk>,
    text: &str,
    mut start: usize,
    end: usize,
    headings: &[String],
    _: bool,
) {
    if text[start..end].trim().is_empty() {
        return;
    }
    let heading = headings
        .iter()
        .filter(|x| !x.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" > ");
    let oversized = end - start > 4096;
    while start < end {
        let mut next = (start + 4096).min(end);
        while !text.is_char_boundary(next) {
            next -= 1;
        }
        // Prefer a complete line for oversized tables/fences; continuation is explicit.
        if next < end
            && let Some(i) = text[start..next].rfind('\n')
            && i > 0
        {
            next = start + i + 1;
        }
        out.push(Chunk {
            start,
            end: next,
            heading: heading.clone(),
            continuation: oversized,
        });
        start = next;
    }
}

/// ATX heading level for a block's first line, using the parser's structural rules.
pub fn heading_level(text: &str) -> Option<usize> {
    let line = text.lines().next()?;
    if line.starts_with('\t') || line.chars().take_while(|c| *c == ' ').count() > 3 {
        return None;
    }
    let text = line.trim();
    let level = text.chars().take_while(|c| *c == '#').count();
    (level > 0
        && level <= 6
        && text
            .as_bytes()
            .get(level)
            .is_some_and(u8::is_ascii_whitespace))
    .then_some(level)
}
