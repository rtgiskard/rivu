# Agent Constraints

## Scope

- MUST stay within the authorized scope and preserve user and other work
- do not revert, clean up, or commit unrelated changes without authorization, and report unrelated findings separately

## Efficiency

- Treat allocation, copying, locking, syscalls, wakeups, and unbounded growth as costs
- Prefer bounded data, batching, event-driven work, and coalescing over polling
- Keep hot and real-time paths deterministic, non-blocking, and allocation-light
- Keep expensive work outside lock scopes and reuse buffers when ownership is clear
- When claiming performance gains, provide a stated workload and baseline; report repeated measurements and variability for timing comparisons, or counts and bytes for deterministic allocation or capacity changes; ordinary internal cleanup does not automatically require benchmarks

## Validation and Evidence

- For Rust source changes, MUST run `make lint` and relevant existing behavior tests; use a throwaway behavior check if no suitable test exists
- `make lint` checks all features, including font embedding; [Makefile](Makefile) defines the checks and [readme.adoc](readme.adoc) documents build and font prerequisites
- For build configuration or dependency changes, choose checks for the affected packages and features; build the relevant artifacts when acceptance depends on linking or release behavior
- When feature behavior or conditional compilation changes, MUST validate affected enabled and disabled combinations; all-features checks miss disabled branches
- For documentation-only changes, check content, links, and command consistency; no lint or build is required
- In parallel work, run shared full checks once after integration; each agent still performs the local reproduction and behavior checks needed for its changes

## Tests

- Permanent tests MUST be deterministic, isolated, independently runnable, and full-suite-safe
- Make environment dependencies explicit and do not rely on developer data or incidental timing

## Commits

- Dependent commit stages MAY be validated together, but MUST state their boundaries and dependencies; do not add compatibility scaffolding or checks solely to validate intermediate commits
- Use a concise Conventional Commit subject; omit the body when the subject is sufficient
- Keep commit body items concise and grouped by meaningful change; explain non-obvious intent or boundaries without repeating the subject or enumerating the diff
- Use real newlines and `- ` items, without terminal punctuation or blank lines between items; capitalization is optional

## Storage

- Build outputs and large temporary artifacts MUST stay under the repository `.cache/`; treat `/tmp` and other memory-backed filesystems as memory resources, not large-artifact storage
- Before large artifact generation, MUST check available storage and keep cache growth bounded; check RAM/swap pressure before memory-intensive work
- MUST pause large artifact generation when storage is insufficient until safe cleanup or additional storage resolves it
- Clean up only confirmed task-owned artifacts that are no longer in use; preserve shared artifacts
