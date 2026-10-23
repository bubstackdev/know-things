//! Bounded context ordering for answers; retrieval ranks and stored spans stay intact.
use crate::contracts::Evidence;
use std::collections::BTreeSet;

fn terms(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .filter(|word| {
            !matches!(
                word.as_str(),
                "a" | "an"
                    | "and"
                    | "are"
                    | "as"
                    | "at"
                    | "be"
                    | "before"
                    | "by"
                    | "can"
                    | "do"
                    | "does"
                    | "for"
                    | "from"
                    | "how"
                    | "i"
                    | "if"
                    | "in"
                    | "into"
                    | "is"
                    | "it"
                    | "its"
                    | "of"
                    | "on"
                    | "or"
                    | "should"
                    | "that"
                    | "the"
                    | "their"
                    | "them"
                    | "there"
                    | "these"
                    | "this"
                    | "to"
                    | "use"
                    | "was"
                    | "were"
                    | "what"
                    | "when"
                    | "where"
                    | "which"
                    | "who"
                    | "why"
                    | "will"
                    | "with"
                    | "would"
            )
        })
        .collect()
}

fn document(e: &Evidence) -> (&str, &str, &str, &str) {
    let r = &e.reference;
    (
        &r.collection_id,
        &r.document_id,
        &r.revision_id,
        &r.content_hash,
    )
}

/// One complete sentence-sized item from an omitted flat Markdown list in a
/// retained document. Reject lead-ins, nesting, code and continuation chunks:
/// they can carry conditions outside the proposed item. The caller admits the
/// whole proposal only when all budgets fit, without displacing retained spans.
pub(crate) fn coverage_excerpt(
    candidates: &[Evidence],
    retained: &[Evidence],
    question: &str,
) -> Option<Evidence> {
    let mut covered = BTreeSet::new();
    for e in retained {
        covered.extend(terms(&e.text));
        covered.extend(terms(&e.source));
        covered.extend(terms(&e.heading_path));
    }
    let missing: BTreeSet<_> = terms(question).difference(&covered).cloned().collect();
    if missing.is_empty() {
        return None;
    }
    let mut best: Option<(usize, Evidence)> = None;
    for candidate in candidates {
        if !candidate.full_chunk
            || candidate.continuation
            || candidate.text.contains("```")
            || candidate.text.contains("~~~")
            || !retained
                .iter()
                .any(|e| document(e) == document(candidate) && e.source == candidate.source)
            || candidate
                .text
                .lines()
                .filter(|s| !s.trim().is_empty())
                .any(|s| !s.starts_with("- "))
        {
            continue;
        }
        let mut offset = 0;
        for line in candidate.text.split_inclusive('\n') {
            let start = candidate.reference.byte_start + offset;
            offset += line.len();
            if !line.starts_with("- ")
                || line.len() > 512
                || !line.trim_end().ends_with('.')
                || retained.iter().any(|e| {
                    document(e) == document(candidate)
                        && e.reference.chunk_id == candidate.reference.chunk_id
                        && e.reference.byte_start <= start
                        && e.reference.byte_end >= start + line.len()
                })
            {
                continue;
            }
            let coverage = terms(line).intersection(&missing).count();
            if coverage == 0 || best.as_ref().is_some_and(|(score, _)| *score >= coverage) {
                continue;
            }
            let mut excerpt = candidate.clone();
            excerpt.text = line.to_owned();
            excerpt.reference.byte_start = start;
            excerpt.reference.byte_end = start + line.len();
            excerpt.full_chunk = false;
            excerpt.continuation = offset != line.len();
            best = Some((coverage, excerpt));
        }
    }
    best.map(|(_, e)| e)
}

/// Propose omitted immediate context for a retained best-ranked passage. Metadata
/// participates in term coverage so repeating a document/section name adds nothing.
/// The caller must recheck budgets and preserve every previously selected span.
/// The proposal contains every retained candidate, plus omitted adjacent context.
/// It never reads or invents source content. No recursive neighbor expansion.
pub(crate) fn prioritize_context(
    evidence: &mut Vec<Evidence>,
    question: &str,
    retained_count: usize,
) -> bool {
    let question = terms(question);
    if question.is_empty() || retained_count == 0 || retained_count >= evidence.len() {
        return false;
    }
    let content: Vec<_> = evidence.iter().map(|e| terms(&e.text)).collect();
    let mut documents = BTreeSet::new();
    let mut used = vec![false; evidence.len()];
    let mut order = Vec::with_capacity(evidence.len());
    for (i, anchor) in evidence.iter().take(retained_count).enumerate() {
        if used[i] {
            continue;
        }
        let mut adjacent = [None, Some(i), None];
        if documents.insert(document(anchor)) {
            let mut covered = content[i].clone();
            covered.extend(terms(&anchor.source));
            covered.extend(terms(&anchor.heading_path));
            for (j, candidate) in evidence.iter().enumerate() {
                if j < retained_count
                    || used[j]
                    || document(candidate) != document(anchor)
                    || candidate.source != anchor.source
                    || candidate.heading_path != anchor.heading_path
                    || (candidate.text.trim().lines().count() == 1
                        && crate::parser::heading_level(&candidate.text).is_some())
                    || !content[j]
                        .difference(&covered)
                        .any(|term| question.contains(term))
                {
                    continue;
                }
                if candidate.reference.byte_end == anchor.reference.byte_start
                    && adjacent[0].is_none()
                {
                    adjacent[0] = Some(j);
                }
                if candidate.reference.byte_start == anchor.reference.byte_end
                    && adjacent[2].is_none()
                {
                    adjacent[2] = Some(j);
                }
            }
        }
        for j in adjacent.into_iter().flatten() {
            if !used[j] {
                used[j] = true;
                order.push(j);
            }
        }
    }
    if order.len() > retained_count {
        *evidence = order.into_iter().map(|i| evidence[i].clone()).collect();
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::Reference;

    fn passage(document: &str, start: usize, text: &str) -> Evidence {
        Evidence {
            reference: Reference {
                collection_id: "collection".into(),
                document_id: document.into(),
                revision_id: "revision".into(),
                chunk_id: format!("{document}-{start}"),
                content_hash: "hash".into(),
                byte_start: start,
                byte_end: start + text.len(),
            },
            source: format!("{document}.md"),
            heading_path: "Guide".into(),
            text: text.into(),
            token_count: text.len(),
            full_chunk: true,
            continuation: false,
            fusion_score: 0.01,
        }
    }

    #[test]
    fn excerpt_preserves_exact_utf8_crlf_span_and_all_original_candidate_fields() {
        let anchor = passage("guide", 0, "Peek queued messages.\n");
        let text = "- Other évidence.\r\n- Search logs with logs_query.\r\n- Extra background.\r\n";
        let candidate = passage("guide", 100, text);
        let original = serde_json::to_value(&candidate).unwrap();
        let excerpt = coverage_excerpt(
            std::slice::from_ref(&candidate),
            std::slice::from_ref(&anchor),
            "Search logs and peek messages?",
        )
        .unwrap();
        let start = text.find("- Search").unwrap();
        assert_eq!(excerpt.text, "- Search logs with logs_query.\r\n");
        assert_eq!(excerpt.reference.byte_start, 100 + start);
        assert_eq!(excerpt.reference.byte_end, 100 + start + excerpt.text.len());
        assert_eq!(excerpt.reference.chunk_id, candidate.reference.chunk_id);
        assert_eq!(
            excerpt.reference.content_hash,
            candidate.reference.content_hash
        );
        assert!(!excerpt.full_chunk);
        assert!(excerpt.continuation);
        assert_eq!(serde_json::to_value(&candidate).unwrap(), original);
    }

    #[test]
    fn excerpt_rejects_intro_conditions_nesting_fences_and_continuation() {
        let anchor = passage("guide", 0, "Cache guide.\n");
        for text in [
            "Only when enabled:\n- Search logs.\n",
            "- Search logs.\n  Only when enabled.\n",
            "```\n- Search logs.\n```",
            "- Search logs\n",
        ] {
            assert!(
                coverage_excerpt(
                    &[passage("guide", 100, text)],
                    std::slice::from_ref(&anchor),
                    "Search logs?"
                )
                .is_none(),
                "{text}"
            );
        }
        let mut candidate = passage("guide", 100, "- Search logs.\n");
        candidate.continuation = true;
        assert!(coverage_excerpt(&[candidate], &[anchor], "Search logs?").is_none());
    }

    #[test]
    fn excerpt_requires_new_terms_same_document_version_and_uncovered_span() {
        let anchor = passage("guide", 0, "Search logs.\n");
        let candidate = passage("guide", 100, "- Search logs.\n");
        assert!(
            coverage_excerpt(std::slice::from_ref(&candidate), &[anchor], "Search logs?").is_none()
        );
        for field in ["revision", "collection", "hash", "document"] {
            let anchor = passage("guide", 0, "Peek messages.\n");
            let mut changed = candidate.clone();
            match field {
                "revision" => changed.reference.revision_id = "other".into(),
                "collection" => changed.reference.collection_id = "other".into(),
                "hash" => changed.reference.content_hash = "other".into(),
                _ => changed.reference.document_id = "other".into(),
            }
            assert!(coverage_excerpt(&[changed], &[anchor], "Search logs?").is_none());
        }
        assert!(
            coverage_excerpt(
                std::slice::from_ref(&candidate),
                std::slice::from_ref(&candidate),
                "Search logs?"
            )
            .is_none()
        );
    }

    #[test]
    fn excerpt_is_one_bounded_item_preferring_coverage_then_rank() {
        let anchor = passage("guide", 0, "Other context.\n");
        let one = passage("guide", 100, "- Search only.\n");
        let both = passage("guide", 200, "- Search logs.\n");
        let tied = passage("guide", 300, "- Logs search.\n");
        assert_eq!(
            coverage_excerpt(
                &[one, both.clone(), tied],
                std::slice::from_ref(&anchor),
                "Search logs?"
            )
            .unwrap()
            .reference,
            both.reference
        );
        let too_long = passage(
            "guide",
            100,
            &format!("- Search logs {}.\n", "x".repeat(512)),
        );
        assert!(coverage_excerpt(&[too_long], &[anchor], "Search logs?").is_none());
    }

    #[test]
    fn adjacent_answer_beats_unrelated_passages_without_changing_source_bytes() {
        let intro = passage("browser", 0, "Browser automation.\n");
        let policy = passage(
            "browser",
            intro.reference.byte_end,
            "Clicks change state.\n",
        );
        let mut ranked = vec![
            passage("monitoring", 0, "Monitoring is read-only.\n"),
            intro.clone(),
            passage("browser", 100, "Screenshots and accessibility snapshots.\n"),
            policy.clone(),
        ];
        let before = serde_json::to_value(&ranked).unwrap();
        prioritize_context(&mut ranked, "Can browser interactions change state?", 2);
        assert_eq!(ranked[1].reference, intro.reference);
        assert_eq!(ranked[2].reference, policy.reference);
        let after = serde_json::to_value(&ranked).unwrap();
        // Every field, including exact spans, continuation and fusion scores, survives.
        for row in before.as_array().unwrap().iter().take(2) {
            assert!(after.as_array().unwrap().contains(row));
        }
        assert_eq!(after[2], before[3]);
        assert_eq!(ranked.len(), 3);
    }

    #[test]
    fn shared_words_and_function_words_do_not_promote_background() {
        let anchor = passage("guide", 0, "Cache policy is Redis.\n");
        let neighbor = passage(
            "guide",
            anchor.reference.byte_end,
            "The cache is on the shelf.\n",
        );
        let mut ranked = vec![anchor, passage("other", 0, "Other evidence."), neighbor];
        let before = serde_json::to_value(&ranked).unwrap();
        prioritize_context(&mut ranked, "What is the cache policy?", 2);
        assert_eq!(before, serde_json::to_value(ranked).unwrap());
    }

    #[test]
    fn context_never_crosses_revision_collection_or_section_boundaries() {
        let anchor = passage("guide", 0, "Cache configuration.\n");
        for field in ["revision", "collection", "hash", "section", "source"] {
            let mut neighbor =
                passage("guide", anchor.reference.byte_end, "Writes mutate state.\n");
            match field {
                "revision" => neighbor.reference.revision_id = "different".into(),
                "collection" => neighbor.reference.collection_id = "different".into(),
                "hash" => neighbor.reference.content_hash = "different".into(),
                "section" => neighbor.heading_path = "Different section".into(),
                "source" => neighbor.source = "different.md".into(),
                _ => unreachable!(),
            }
            let mut ranked = vec![
                anchor.clone(),
                passage("other", 0, "Other evidence."),
                neighbor,
            ];
            let before = serde_json::to_value(&ranked).unwrap();
            prioritize_context(&mut ranked, "Can cache writes mutate state?", 2);
            assert_eq!(before, serde_json::to_value(ranked).unwrap(), "{field}");
        }
    }

    #[test]
    fn context_is_bounded_to_one_neighbor_without_recursive_chaining() {
        let anchor = passage("guide", 0, "Cache guide.\n");
        let next = passage("guide", anchor.reference.byte_end, "Writes mutate state.\n");
        let later = passage(
            "guide",
            next.reference.byte_end,
            "Expiration removes keys.\n",
        );
        let other = passage("other", 0, "Other evidence.");
        let mut ranked = vec![anchor, other.clone(), later.clone(), next.clone()];
        prioritize_context(
            &mut ranked,
            "How do writes mutate state and expiration remove keys?",
            2,
        );
        assert_eq!(ranked[1].reference, next.reference);
        assert_eq!(ranked[2].reference, other.reference);
        assert!(!ranked.iter().any(|e| e.reference == later.reference));
    }

    #[test]
    fn preceding_conditions_keep_source_order_and_identifier_matching() {
        let condition = passage("guide", 0, "Only when CACHE_KEY is absent.\n");
        let anchor = passage("guide", condition.reference.byte_end, "Use the fallback.\n");
        let mut ranked = vec![
            anchor.clone(),
            passage("other", 0, "Other evidence."),
            condition.clone(),
        ];
        prioritize_context(&mut ranked, "When does cache_key allow fallback?", 2);
        assert_eq!(ranked[0].reference, condition.reference);
        assert_eq!(ranked[1].reference, anchor.reference);
    }

    #[test]
    fn already_selected_neighbors_keep_the_original_order() {
        let anchor = passage("guide", 0, "Track master and SHA pinning.\n");
        let neighbor = passage(
            "guide",
            anchor.reference.byte_end,
            "No tag-based versioning.\n",
        );
        let mut ranked = vec![
            anchor,
            passage("other", 0, "Other evidence."),
            neighbor,
            passage("omitted", 0, "Additional background."),
        ];
        let before = serde_json::to_value(&ranked).unwrap();
        assert!(!prioritize_context(
            &mut ranked,
            "Are tag or SHA refs used?",
            3
        ));
        assert_eq!(before, serde_json::to_value(ranked).unwrap());
    }

    #[test]
    fn repeating_section_names_is_not_new_question_coverage() {
        let mut anchor = passage("queues", 0, "Peek messages without consuming them.\n");
        anchor.heading_path = "RabbitMQ tools".into();
        let mut neighbor = passage("queues", anchor.reference.byte_end, "Mirrors RabbitMQ.\n");
        neighbor.heading_path = anchor.heading_path.clone();
        let mut ranked = vec![
            anchor,
            passage("logs", 0, "Search application logs."),
            neighbor,
        ];
        let before = serde_json::to_value(&ranked).unwrap();
        assert!(!prioritize_context(
            &mut ranked,
            "Search logs and inspect RabbitMQ messages?",
            2
        ));
        assert_eq!(before, serde_json::to_value(ranked).unwrap());
    }
}
