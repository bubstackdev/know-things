# Know-Things design

Know-Things retrieves documentation evidence and generates cited answers on-device.
Accuracy, source traceability, lifecycle correctness and request latency are joint goals.
Source-code analysis belongs to a separate project.

The accepted foundation is Rust, SQLite FTS5 plus exact vector retrieval, and a
persistent local inference worker. Sources remain external and read-only. Immutable
revision bytes and references provide the authority for every returned citation.
Models, runtime libraries and tokenizers are verified content-pinned assets; inference
has no cloud fallback. Deployments use captured artifacts rather than mutable checkouts.

The implementation supports lexical and hybrid retrieval, bounded context expansion,
streamed answers, cancellation, source exclusions, reconciliation, backup/restore,
revision retention and explicit index recovery. Optional background maintenance stays
bounded and shares the foreground ownership rules.

The current local validation summary demonstrates sub-two-second p95 first-visible
output on one workstation and workload. Broader datasets, independent review, resource
tiers, installation coverage and redistribution licensing remain qualification work.
Private benchmark sources, service descriptions and research records are excluded.

The maintained design is divided into:

- [Architecture](docs/ARCHITECTURE.md): ownership, storage, retrieval and runtime decisions.
- [Contracts](docs/CONTRACTS.md): source identities, protocol and failure semantics.
- [Corpus handling](docs/CORPUS.md): external source and fixture requirements.
- [Evaluation](docs/EVALUATION.md): reproducible capture, review and measurement.
- [Validation](docs/VALIDATION.md): observed results, checks and remaining release gates.
