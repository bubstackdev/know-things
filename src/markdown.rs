use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::ops::Range;

pub const PASSAGE_BYTES: usize = 1_800;
pub const PASSAGE_LINES: usize = 24;

#[derive(Debug)]
pub struct Passage<'a> {
    pub heading: String,
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub body: &'a str,
}

pub fn line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    offsets.extend(text.match_indices('\n').map(|(index, _)| index + 1));
    if offsets.last().copied() != Some(text.len()) {
        offsets.push(text.len());
    }
    offsets
}

fn lines_for(offsets: &[usize], bytes: Range<usize>) -> Range<usize> {
    let start = offsets.partition_point(|&at| at <= bytes.start) - 1;
    let end = offsets.partition_point(|&at| at < bytes.end);
    start..end
}

fn fits(offsets: &[usize], start: usize, end: usize) -> bool {
    end - start <= PASSAGE_LINES && offsets[end] - offsets[start] <= PASSAGE_BYTES
}

struct Collector<'a> {
    text: &'a str,
    offsets: Vec<usize>,
    ancestors: Vec<(HeadingLevel, String)>,
    active: Option<Range<usize>>,
    passages: Vec<Passage<'a>>,
}

impl<'a> Collector<'a> {
    fn flush(&mut self) {
        if let Some(lines) = self.active.take() {
            self.push(lines);
        }
    }

    fn push(&mut self, lines: Range<usize>) {
        let body = &self.text[self.offsets[lines.start]..self.offsets[lines.end]];
        if !body.trim().is_empty() {
            self.passages.push(Passage {
                heading: self
                    .ancestors
                    .iter()
                    .map(|(_, title)| title.as_str())
                    .collect::<Vec<_>>()
                    .join(" > "),
                start_line: lines.start + 1,
                end_line: lines.end,
                start_byte: self.offsets[lines.start],
                body,
            });
        }
    }

    fn add(&mut self, bytes: Range<usize>) {
        if bytes.is_empty() {
            return;
        }
        let lines = lines_for(&self.offsets, bytes);
        if let Some(active) = &mut self.active {
            if fits(&self.offsets, active.start, lines.end) {
                active.end = lines.end;
                return;
            }
            self.flush();
        }
        let mut start = lines.start;
        while start < lines.end {
            let mut end = start + 1;
            while end < lines.end && fits(&self.offsets, start, end + 1) {
                end += 1;
            }
            if end == lines.end {
                self.active = Some(start..end);
            } else {
                self.push(start..end);
            }
            start = end;
        }
    }
}

pub fn parse<'a>(path: &str, text: &'a str) -> (String, Vec<Passage<'a>>) {
    let mut collector = Collector {
        text,
        offsets: line_offsets(text),
        ancestors: Vec::new(),
        active: None,
        passages: Vec::new(),
    };
    let mut title = None;
    let mut heading = None;
    let mut depth = 0;
    let mut metadata_end = 0;
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    for (event, bytes) in Parser::new_ext(text, options).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    if let Tag::Heading { level, .. } = tag {
                        collector.flush();
                        heading = Some((level, String::new()));
                    } else if matches!(tag, Tag::MetadataBlock(_)) {
                        metadata_end = bytes.end;
                    } else {
                        collector.add(bytes);
                    }
                }
                depth += 1;
            }
            Event::End(tag) => {
                depth -= 1;
                if depth == 0
                    && matches!(tag, TagEnd::Heading(_))
                    && let Some((level, label)) = heading.take()
                {
                    let label: String = label.chars().take(160).collect();
                    if level == HeadingLevel::H1 && title.is_none() && !label.is_empty() {
                        title = Some(label.clone());
                    }
                    collector.ancestors.retain(|(parent, _)| *parent < level);
                    collector.ancestors.push((level, label));
                }
            }
            Event::Text(value) | Event::Code(value) => {
                if let Some((_, label)) = &mut heading {
                    label.push_str(&value);
                } else if depth == 0 {
                    collector.add(bytes);
                }
            }
            _ if depth == 0 => collector.add(bytes),
            _ => {}
        }
    }
    collector.flush();
    if collector.passages.is_empty() && !text[metadata_end..].trim().is_empty() {
        collector.add(metadata_end..text.len());
        collector.flush();
    }
    (title.unwrap_or_else(|| path.to_owned()), collector.passages)
}

pub fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|part| !part.is_empty())
}

pub fn identifier_parts(text: &str) -> String {
    let mut parts = String::new();
    for word in words(text) {
        let mut start = 0;
        let mut previous = None;
        for (index, ch) in word.char_indices() {
            if ch.is_uppercase() && previous.is_some_and(char::is_lowercase) {
                parts.push_str(&word[start..index]);
                parts.push(' ');
                start = index;
            }
            previous = Some(ch);
        }
        if start > 0 {
            parts.push_str(&word[start..]);
            parts.push(' ');
        }
    }
    parts
}
