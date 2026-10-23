# Working on Know-Things

Read [README.md](README.md) for the documentation map. Before implementation, read the architecture and contracts; use the validation plan to choose the next milestone. Update the owning document when a material decision changes.

## Confirmed product direction

- Know-Things focuses on documentation. the sibling `../knowcode` project owns source-code understanding.
- Documentation sources are configured per installation. Treat them as read-only; perform lifecycle tests on disposable copies. See [docs/CORPUS.md](docs/CORPUS.md) for corpus handling and evaluation scope.
- Everything runs on-device, including embeddings and answer generation. No cloud inference fallback.
- The accepted starting architecture is Rust, SQLite FTS5, vector retrieval, and a persistent Flash worker. Specific models and any ANN backend require qualification.
- `../flash` is a sibling project with ongoing work. Inspect it read-only unless changes there are explicitly authorized. Release integration must use a pinned artifact rather than an implicit dependency on its mutable checkout.
- Prefer simple ownership and transactional correctness. Add index/runtime complexity only for a demonstrated requirement and include its recovery/upgrade path.

## Protect other sessions on this computer

Permanent user instruction, 2026-10-15: Never suspend or interfere with the user's other terminal sessions or their processes, in any session on this computer.

- Never send SIGSTOP/SIGTSTP, stop or terminate another session's jobs, or change its terminal state to obtain benchmark isolation or free resources.
- Control only jobs started by the current session. If other work competes for resources, wait, reduce or stop your own workload, or report the contention.
- Broad statements such as "the machine is yours" do not authorize disrupting other sessions. Do not reuse the prior process-isolation approach or create automatic suspension timers.
