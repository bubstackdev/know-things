//! Render Markdown inline-code delimiters without changing their literal contents.
//! Canonical evidence, references, and conservative selection costs stay byte-exact.
pub(crate) fn inline_code_text(text: &str) -> String {
    // Fenced/nested code deserves its original representation. Do not interpret it.
    if text.contains("```") || text.contains("~~~") {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if line.starts_with("    ") || line.starts_with('\t') {
            out.push_str(line);
            continue;
        }
        let bytes = line.as_bytes();
        let mut cursor = 0;
        let mut copied = 0;
        while cursor < bytes.len() {
            if bytes[cursor] != b'`' {
                cursor += 1;
                continue;
            }
            let escaped = bytes[..cursor]
                .iter()
                .rev()
                .take_while(|b| **b == b'\\')
                .count()
                % 2
                == 1;
            let width = bytes[cursor..].iter().take_while(|b| **b == b'`').count();
            if escaped {
                cursor += width;
                continue;
            }
            let mut end = cursor + width;
            let mut close = None;
            while end < bytes.len() {
                if bytes[end] != b'`' {
                    end += 1;
                    continue;
                }
                let run = bytes[end..].iter().take_while(|b| **b == b'`').count();
                if run == width {
                    close = Some(end);
                    break;
                }
                end += run;
            }
            if let Some(end) = close {
                out.push_str(&line[copied..cursor]);
                out.push_str(&line[cursor + width..end]);
                cursor = end + width;
                copied = cursor;
            } else {
                cursor += width;
            }
        }
        out.push_str(&line[copied..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::inline_code_text;
    #[test]
    fn strips_paired_delimiters_preserving_values_unicode_and_line_endings() {
        assert_eq!(
            inline_code_text("Use `IDatabase` and ``a`b``; `é <= 2`.\r\n"),
            "Use IDatabase and a`b; é <= 2.\r\n"
        );
        assert_eq!(inline_code_text("`x`+`y` != `z`"), "x+y != z");
    }
    #[test]
    fn leaves_fences_indentation_escaped_and_unmatched_ticks_unchanged() {
        for text in [
            "```cs\nvar x = `literal`;\n```\n",
            "~~~\n`literal`\n~~~",
            "    `literal`\r\n",
            "\t`literal`\n",
            "\\`literal\\`",
            "unclosed `code\nnext line`",
            "``unclosed`",
            "> ```\n`literal`\n> ```",
        ] {
            assert_eq!(inline_code_text(text), text);
        }
    }
}
