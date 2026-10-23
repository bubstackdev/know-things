# Documentation corpora

Configure the source directory per installation. Source documents are external,
read-only inputs; never modify, move or vendor a user's knowledge base during import.
Store indexes, revisions and evaluation outputs outside the source directory.

The initial supported formats are Markdown and UTF-8 text, including headings,
procedures, code fences, configuration examples, tables and lists. Code examples remain
documentation evidence; source-code analysis is outside this project's scope.

## Private manifests and labels

Keep source manifests, document names, hashes, repository revisions, internal links,
questions, required claims, exact support spans and detailed results under `.private/`.
A manifest identifies a complete source snapshot; it is neither a backup nor permission
to publish its contents or metadata. Detect source changes before and after each run.
Retain previous manifests rather than rewriting an experiment's identity.

Labels use half-open UTF-8 byte ranges in immutable hashed source revisions. Include
paraphrases, exact identifiers, procedures, cross-document questions, conflicting rules
and meaningful unknowns. Documentation cannot establish current operational state unless
that state is actually present in the selected snapshot. Record review provenance and
freeze tuning/held-out splits before model or prompt selection.

Do not follow links or execute commands embedded in source documents. Referenced
repositories and services do not automatically become part of a collection.

## Lifecycle and scale

Exercise edits, renames, deletions, exclusions, malformed input and recovery on
disposable fixtures or copies. Never manufacture lifecycle changes in a real source
collection. Symlinks and configured exclusions must be checked before reading files.

Measure actual eligible chunks, source bytes, embedding coverage and native token
counts. Source size is not a token estimate. Use separate synthetic scale workloads
before selecting an approximate index or adding storage/runtime complexity.

Comparisons require identical source bytes, scope, tokenizer budgets and query labels.
When a comparison service cannot expose the same snapshot, label its output as an
uncontrolled observation and retain it privately.

See [the evaluation workflow](EVALUATION.md) for capture and review, and
[validation](VALIDATION.md) for the anonymous aggregate local result.
