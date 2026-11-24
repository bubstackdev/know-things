use crate::{index::Evidence, markdown::words};
use rust_stemmers::{Algorithm, Stemmer};
use std::{borrow::Cow, collections::HashSet, sync::LazyLock};

const MIN_BODY_OVERLAP_PERCENT: usize = 45;

const COMMON_WORDS: &[&str] = &[
    "about",
    "across",
    "after",
    "again",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "before",
    "by",
    "can",
    "could",
    "currently",
    "default",
    "did",
    "do",
    "does",
    "each",
    "either",
    "else",
    "enough",
    "exact",
    "exactly",
    "first",
    "for",
    "from",
    "get",
    "global",
    "had",
    "happens",
    "has",
    "have",
    "how",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "itself",
    "maximum",
    "me",
    "minimum",
    "more",
    "must",
    "my",
    "need",
    "new",
    "of",
    "on",
    "one",
    "only",
    "or",
    "our",
    "per",
    "same",
    "should",
    "still",
    "takes",
    "tell",
    "that",
    "the",
    "their",
    "there",
    "these",
    "this",
    "to",
    "use",
    "uses",
    "using",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "why",
    "with",
    "would",
    "you",
];

const FOCUS_BOUNDARIES: &[&str] = &[
    "about",
    "after",
    "allows",
    "applies",
    "before",
    "can",
    "chooses",
    "could",
    "did",
    "do",
    "does",
    "enforces",
    "for",
    "implements",
    "in",
    "includes",
    "permits",
    "records",
    "requires",
    "set",
    "sets",
    "should",
    "use",
    "uses",
    "when",
    "while",
    "will",
    "work",
    "would",
];

const MEASURES: &[&str] = &[
    "count",
    "duration",
    "expiration",
    "expiry",
    "interval",
    "limit",
    "period",
    "quota",
    "rate",
    "retention",
    "schedule",
    "size",
    "timeout",
];

const REFERENCE_MARKERS: &[&str] = &["incident", "issue", "story", "ticket"];
const AUTOMATION_KINDS: &[&str] = &["cron", "lambda", "scheduler", "webhook"];
const PROCEDURE_ACTIONS: &[&str] = &[
    "archive", "delete", "disable", "enable", "refresh", "restart", "revoke", "rotate", "upload",
];

pub struct EvidenceQuery {
    terms: HashSet<String>,
    focus: HashSet<String>,
    exact: HashSet<String>,
    references: HashSet<String>,
    measures: HashSet<&'static str>,
    measure_focus: bool,
    automation: Option<&'static str>,
    procedure: Option<&'static str>,
    asks_for_route: bool,
}

impl EvidenceQuery {
    pub fn new(query: &str) -> Self {
        let tokens: Vec<_> = words(query).collect();
        let lower: Vec<_> = tokens.iter().map(|token| token.to_lowercase()).collect();
        let terms = lower
            .iter()
            .filter_map(|word| significant_term(word).map(Cow::into_owned))
            .collect();
        let focus_words = request_focus(&lower);
        let focus = focus_words
            .iter()
            .filter_map(|word| significant_term(word).map(Cow::into_owned))
            .collect();
        let measures = requested_measures(&lower).collect();
        let measure_focus = requested_measures(focus_words).next().is_some();
        let exact = tokens
            .iter()
            .filter(|word| is_identifier(word))
            .map(|word| word.to_lowercase())
            .chain(lower.windows(2).filter_map(|pair| {
                let value = &pair[1];
                (REFERENCE_MARKERS.contains(&pair[0].as_str())
                    && value.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| value.clone())
            }))
            .collect();
        let references = references(query)
            .filter(|(prefix, _)| {
                prefix.bytes().all(|byte| byte.is_ascii_uppercase())
                    || prefix.eq_ignore_ascii_case("sc")
            })
            .map(|(_, key)| key.to_ascii_lowercase())
            .collect();
        let mut automation = None;
        let mut asks_for_route = false;
        let mut procedure = None;
        match lower.first().map(String::as_str) {
            Some("what" | "which") => {
                let head = &lower[1..lower.len().min(7)];
                automation = head
                    .iter()
                    .find_map(|word| AUTOMATION_KINDS.iter().copied().find(|kind| kind == word));
                asks_for_route = head
                    .iter()
                    .any(|word| matches!(word.as_str(), "endpoint" | "route"))
                    || (head.iter().any(|word| word == "operation")
                        && head.iter().any(|word| word == "swagger"));
            }
            Some("where" | "how") => {
                procedure = lower.iter().find_map(|word| {
                    PROCEDURE_ACTIONS
                        .iter()
                        .copied()
                        .find(|action| action == word)
                });
            }
            _ => {}
        }
        Self {
            terms,
            focus,
            exact,
            references,
            measures,
            measure_focus,
            automation,
            procedure,
            asks_for_route,
        }
    }

    pub fn accepts(&self, hits: &[Evidence]) -> bool {
        if self.terms.len() <= 2 && self.exact.is_empty() && self.references.is_empty() {
            return true;
        }
        let mut body = HashSet::new();
        let mut exact = HashSet::new();
        let mut found_references = HashSet::new();
        for hit in hits {
            for word in words(&hit.markdown) {
                let lower = word.to_lowercase();
                if let Some(term) = self.terms.get(fold_term(&lower).as_ref()) {
                    body.insert(term.as_str());
                }
            }
            if exact.len() < self.exact.len() {
                exact.extend(self.exact.iter().filter(|key| {
                    identifier_matches(&hit.markdown, key)
                        || identifier_matches(&hit.title, key)
                        || identifier_matches(&hit.section, key)
                }));
            }
            if found_references.len() < self.references.len() {
                found_references.extend(
                    references(&hit.markdown)
                        .chain(references(&hit.section))
                        .map(|(_, key)| key.to_ascii_lowercase())
                        .filter(|key| self.references.contains(key)),
                );
            }
        }
        if exact.len() != self.exact.len() || found_references.len() != self.references.len() {
            return false;
        }
        if !self.measures.is_empty()
            && (self.measure_focus || !self.exact.is_empty())
            && !hits.iter().any(|hit| self.supports_measure(hit))
        {
            return false;
        }
        if let Some(kind) = self.automation
            && !hits.iter().any(|hit| self.supports_automation(hit, kind))
        {
            return false;
        }
        if self.asks_for_route && hits.iter().any(|hit| self.supports_short_route(hit)) {
            return true;
        }
        // Keep short topic and identifier lookups usable. Longer questions need support
        // in the returned bodies. Titles help rank them but cannot establish that support.
        if self.terms.len() > 2 && body.len() * 100 < self.terms.len() * MIN_BODY_OVERLAP_PERCENT {
            return false;
        }
        if self.focus.len() < 2 {
            return true;
        }
        let covered = self
            .focus
            .iter()
            .filter(|term| body.contains(term.as_str()))
            .count();
        covered * 2 >= self.focus.len()
    }

    fn supports_measure(&self, hit: &Evidence) -> bool {
        let terms = lookup_terms(&hit.markdown);
        self.measures
            .iter()
            .all(|measure| terms.contains(fold_term(measure).as_ref()))
            && (!self.measure_focus || self.focus.is_subset(&terms))
            && self.exact.iter().all(|key| {
                identifier_matches(&hit.markdown, key)
                    || identifier_matches(&hit.section, key)
                    || identifier_matches(&hit.title, key)
            })
    }

    pub fn looks_like_procedure(&self, hit: &Evidence) -> bool {
        let Some(action) = self.procedure else {
            return false;
        };
        let mentions_action = |text: &str| {
            words(text).any(|word| {
                word.eq_ignore_ascii_case(action)
                    || word
                        .strip_suffix('s')
                        .is_some_and(|verb| verb.eq_ignore_ascii_case(action))
            })
        };
        mentions_action(&hit.section)
            && mentions_action(&hit.markdown)
            && self.context_term_count([hit.title.as_str(), &hit.section, &hit.markdown]) >= 3
    }

    pub fn asks_for_procedure(&self) -> bool {
        self.procedure.is_some()
    }

    fn supports_automation(&self, hit: &Evidence, kind: &str) -> bool {
        let body = lookup_terms(&hit.markdown);
        let kind = fold_term(kind);
        body.contains(kind.as_ref())
            && self
                .terms
                .intersection(&body)
                .filter(|word| {
                    word.as_str() != fold_term("automatically")
                        && word.as_str() != fold_term("scheduled")
                        && word.as_str() != kind
                })
                .take(2)
                .count()
                == 2
    }

    fn supports_short_route(&self, hit: &Evidence) -> bool {
        if hit.markdown.len() > 320
            || !hit
                .markdown
                .split(|ch: char| {
                    ch.is_whitespace() || matches!(ch, '<' | '>' | '(' | ')' | '`' | '"')
                })
                .any(is_swagger_operation)
        {
            return false;
        }
        let mut body = lookup_terms(&hit.markdown);
        if body.contains(fold_term("stage").as_ref()) {
            body.insert(fold_term("staging").into_owned());
        }
        if ["staging", "production"]
            .iter()
            .map(|env| fold_term(env))
            .any(|env| self.terms.contains(env.as_ref()) && !body.contains(env.as_ref()))
        {
            return false;
        }
        let title = lookup_terms(&hit.title);
        let in_body = self.terms.intersection(&body).count();
        let in_title_only = self
            .terms
            .intersection(&title)
            .filter(|word| !body.contains(*word))
            .count();
        in_body >= 2
            && in_title_only >= 2
            && (in_body + in_title_only) * 100 >= self.terms.len() * MIN_BODY_OVERLAP_PERCENT
    }

    pub fn context_term_count<'a>(&self, excerpts: impl IntoIterator<Item = &'a str>) -> usize {
        excerpts
            .into_iter()
            .flat_map(words)
            .filter_map(|word| self.terms.get(fold_term(&word.to_lowercase()).as_ref()))
            .collect::<HashSet<_>>()
            .len()
    }
}

pub(crate) fn lookup_terms(text: &str) -> HashSet<String> {
    words(text)
        .map(str::to_lowercase)
        .filter_map(|word| significant_term(&word).map(Cow::into_owned))
        .collect()
}

fn is_swagger_operation(candidate: &str) -> bool {
    url::Url::parse(candidate).is_ok_and(|link| {
        matches!(link.scheme(), "http" | "https")
            && link
                .path_segments()
                .is_some_and(|mut segments| segments.any(|part| part == "swagger"))
            && link
                .fragment()
                .and_then(|fragment| fragment.strip_prefix('/'))
                .and_then(|fragment| fragment.split_once('/'))
                .is_some_and(|(section, operation)| !section.is_empty() && !operation.is_empty())
    })
}

fn request_focus(tokens: &[String]) -> &[String] {
    let Some((first, mut rest)) = tokens.split_first() else {
        return &[];
    };
    if !matches!(first.as_str(), "what" | "which") {
        return &[];
    }
    if rest
        .first()
        .is_some_and(|word| matches!(word.as_str(), "is" | "are" | "was" | "were"))
    {
        rest = &rest[1..];
    }
    if rest
        .first()
        .is_some_and(|word| matches!(word.as_str(), "the" | "our" | "a"))
    {
        rest = &rest[1..];
    }
    // Only use a short, identifiable question head. Free-form prose uses whole-question coverage.
    match rest
        .iter()
        .position(|word| FOCUS_BOUNDARIES.contains(&word.as_str()))
    {
        Some(end @ 1..=6) => &rest[..end],
        _ => &[],
    }
}

fn requested_measures(tokens: &[String]) -> impl Iterator<Item = &'static str> {
    MEASURES.iter().copied().filter(|measure| {
        tokens.iter().any(|word| {
            word == measure
                || word
                    .strip_suffix('s')
                    .is_some_and(|singular| singular == *measure)
        })
    })
}

pub(crate) fn significant_term(word: &str) -> Option<Cow<'_, str>> {
    ((2..=128).contains(&word.len()) && COMMON_WORDS.binary_search(&word).is_err())
        .then(|| fold_term(word))
}

pub(crate) fn fold_term(word: &str) -> Cow<'_, str> {
    static STEMMER: LazyLock<Stemmer> = LazyLock::new(|| Stemmer::create(Algorithm::English));
    STEMMER.stem(word)
}

pub(crate) fn identifier_matches(text: &str, key: &str) -> bool {
    text.split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .any(|word| {
            word.eq_ignore_ascii_case(key)
                || word.contains('_') && word.replace('_', "").eq_ignore_ascii_case(key)
        })
}

pub fn is_identifier(word: &str) -> bool {
    let mut letters = word.chars().filter(|ch| ch.is_alphabetic()).peekable();
    letters.peek().is_some()
        && (word.chars().any(|ch| ch.is_ascii_digit())
            || (word.chars().skip(1).any(char::is_uppercase) && letters.any(char::is_lowercase)))
}

fn references(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-')
        .filter_map(|key| {
            let (prefix, suffix) = key.split_once('-')?;
            (prefix.len() >= 2
                && prefix.bytes().all(|byte| byte.is_ascii_alphabetic())
                && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
                && suffix.bytes().any(|byte| byte.is_ascii_digit()))
            .then_some((prefix, key))
        })
}
