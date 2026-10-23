use know_things::parser;
#[test]
fn fences_tables_and_crlf_preserve_source_spans_and_heading_paths() {
    let raw = "# Guide\r\n\r\n```markdown\r\n# Inside fence\r\n\r\nexample\r\n```\r\n\r\n| A | B |\r\n|---|---|\r\n| 1 | 2 |\r\n\r\n# Next\r\n\r\nTail.\r\n";
    let chunks = parser::parse(raw, true).unwrap();
    let fence = chunks
        .iter()
        .find(|c| raw[c.start..c.end].contains("```markdown"))
        .unwrap();
    let text = &raw[fence.start..fence.end];
    assert!(text.contains("# Inside fence"));
    assert!(text.contains("example\r\n```"));
    assert_eq!(fence.heading, "Guide");
    let table = chunks
        .iter()
        .find(|c| raw[c.start..c.end].contains("| A | B |"))
        .unwrap();
    assert!(raw[table.start..table.end].contains("| 1 | 2 |"));
    assert_eq!(table.heading, "Guide");
    assert_eq!(chunks.last().unwrap().heading, "Next");
}
#[test]
fn oversized_unicode_blocks_have_explicit_continuations_and_valid_byte_boundaries() {
    let raw = "🦀".repeat(2000);
    let chunks = parser::parse(&raw, true).unwrap();
    assert!(chunks.len() > 1);
    assert!(
        chunks
            .iter()
            .all(|c| c.continuation && c.end - c.start <= 4096)
    );
    let reconstructed = chunks
        .iter()
        .map(|c| &raw[c.start..c.end])
        .collect::<String>();
    assert_eq!(reconstructed, raw);
    assert_eq!(parser::token_count(&raw), 8000);
}
#[test]
fn indented_literals_do_not_create_headings() {
    let raw = "# Actual\n\n    # Literal code heading\n\n\t# Another literal\n";
    let chunks = parser::parse(raw, true).unwrap();
    assert!(chunks.iter().all(|c| c.heading == "Actual"));
    assert!(
        chunks
            .iter()
            .any(|c| raw[c.start..c.end].contains("Literal"))
    );
}

#[test]
fn oversized_heading_metadata_is_rejected_before_chunk_fanout() {
    let source = format!("# {}\n\n{}", "x".repeat(2000), "body\n".repeat(1000));
    assert_eq!(
        parser::parse(&source, true).unwrap_err().code,
        "resource_exhausted"
    );
}
