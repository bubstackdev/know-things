use crate::{
    local_file, source,
    wire::{self, Request, Response, SearchArgs},
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs::File,
    io::{self, BufRead, BufReader, Read},
    path::Path,
    time::Instant,
};

const MAX_INPUT: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Question {
    id: String,
    query: String,
    evidence_sets: Vec<Vec<Span>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Span {
    path: String,
    start_line: usize,
    end_line: usize,
}

fn questions(path: &Path) -> Result<Vec<Question>> {
    let reader: Box<dyn Read> = if path == Path::new("-") {
        Box::new(io::stdin().lock())
    } else {
        Box::new(File::open(path)?)
    };
    let input =
        local_file::read_limited(reader, MAX_INPUT).context("could not read question set")?;
    let mut questions = Vec::new();
    let mut ids = HashSet::new();
    for (index, line) in BufReader::new(input.as_slice()).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let question: Question = serde_json::from_str(&line)
            .map_err(|_| anyhow::anyhow!("invalid test question at line {}", index + 1))?;
        ensure!(
            !question.id.is_empty() && ids.insert(question.id.clone()),
            "each question needs a unique ID"
        );
        for set in &question.evidence_sets {
            ensure!(
                !set.is_empty(),
                "use an empty evidence_sets array for unanswerable questions"
            );
            for span in set {
                ensure!(
                    source::is_markdown_path(&span.path)
                        && span.start_line > 0
                        && span.end_line >= span.start_line,
                    "invalid evidence span at line {}",
                    index + 1
                );
            }
        }
        questions.push(question);
    }
    ensure!(!questions.is_empty(), "question set is empty");
    Ok(questions)
}

fn covered(span: &Span, hits: &[Value]) -> bool {
    let mut intervals: Vec<_> = hits
        .iter()
        .filter_map(|hit| {
            if hit["path"].as_str()? != span.path {
                return None;
            }
            Some((
                hit["start_line"].as_u64()? as usize,
                hit["end_line"].as_u64()? as usize,
            ))
        })
        .collect();
    intervals.sort_unstable();
    let mut next = span.start_line;
    for (start, end) in intervals {
        if start > next {
            break;
        }
        if end >= span.end_line {
            return true;
        }
        next = next.max(end.saturating_add(1));
    }
    false
}

fn rank(question: &Question, hits: &[Value]) -> Option<usize> {
    (1..=hits.len()).find(|&n| {
        question
            .evidence_sets
            .iter()
            .any(|set| set.iter().all(|span| covered(span, &hits[..n])))
    })
}

pub async fn evaluate(
    socket: &Path,
    input: &Path,
    commit: &str,
    repetitions: u16,
) -> Result<Value> {
    ensure!(source::is_oid(commit), "expected an immutable commit ID");
    let questions = questions(input)?;
    let mut scores = Vec::new();
    let mut times = Vec::new();
    let mut answerable = 0;
    let mut complete_at_one = 0;
    let mut complete_at_five = 0;
    let mut unsupported_queries_with_hits = 0;
    for question in &questions {
        let mut first_rank = None;
        let mut returned_hits = false;
        for attempt in 0..=repetitions {
            let request = Request::Search(SearchArgs {
                query: question.query.clone(),
                limit: Some(5),
            });
            let start = Instant::now();
            let response = wire::call(socket, request).await?;
            let runtime = start.elapsed();
            let result = match response {
                Response::Ok(result) => result,
                Response::Error(error) => {
                    anyhow::bail!("evaluation request failed: {}", error.code)
                }
            };
            ensure!(
                result["indexed_commit"].as_str() == Some(commit),
                "index revision changed during evaluation"
            );
            if attempt == 0 {
                let hits = result["hits"]
                    .as_array()
                    .context("retrieval did not return hits")?;
                first_rank = rank(question, hits);
                returned_hits = !hits.is_empty();
            } else {
                times.push(runtime.as_micros());
            }
        }
        if question.evidence_sets.is_empty() {
            unsupported_queries_with_hits += usize::from(returned_hits);
        } else {
            answerable += 1;
            complete_at_one += usize::from(first_rank == Some(1));
            complete_at_five += usize::from(first_rank.is_some());
        }
        scores.push(json!({
            "id": question.id,
            "answerable": !question.evidence_sets.is_empty(),
            "first_complete_rank": first_rank,
            "returned_hits": returned_hits,
        }));
    }
    times.sort_unstable();
    let percentile = |p: usize| times[(times.len() * p).div_ceil(100) - 1];
    Ok(json!({
        "commit": commit,
        "questions": questions.len(),
        "answerable": answerable,
        "complete_at_one": complete_at_one,
        "complete_at_five": complete_at_five,
        "unsupported_queries_with_hits": unsupported_queries_with_hits,
        "latency": {
            "scope": "warm serial local daemon calls excluding MCP startup",
            "samples": times.len(), "p50_us": percentile(50),
            "p95_us": percentile(95), "p99_us": percentile(99),
        },
        "cases": scores,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_requires_complete_spans_from_one_acceptable_evidence_set() {
        let question: Question = serde_json::from_value(json!({
            "id": "example", "query": "test",
            "evidence_sets": [
                [{"path":"a.md","start_line":4,"end_line":7},{"path":"b.md","start_line":8,"end_line":8}],
                [{"path":"alternative.md","start_line":1,"end_line":1}]
            ]
        })).unwrap();
        let hits = [
            json!({"path":"a.md","start_line":4,"end_line":5}),
            json!({"path":"b.md","start_line":8,"end_line":8}),
            json!({"path":"a.md","start_line":6,"end_line":7}),
        ];
        assert_eq!(rank(&question, &hits[..2]), None);
        assert_eq!(rank(&question, &hits), Some(3));
        assert_eq!(
            rank(
                &question,
                &[json!({"path":"alternative.md","start_line":1,"end_line":1})]
            ),
            Some(1)
        );
    }
}
