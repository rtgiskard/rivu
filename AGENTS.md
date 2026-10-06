# Agent Constraints

## Design

- Prefer elegant, boring, KISS solutions over clever abstractions
- Keep boundaries, ownership, failure states, and control flow explicit
- Add abstractions only to remove real duplication or isolate real change
- Reuse existing conventions and delete obsolete paths after a clean cutover
- Avoid compatibility debt, transitional shims, obsolete behavior, and historical baggage; prefer clean cutovers
- Protect correctness, readability, visual consistency, and six-month maintainability
- State the invariant, cost, and tradeoff behind non-obvious choices

## Efficiency

- Treat allocation, copying, locking, syscalls, wakeups, and unbounded growth as costs
- Prefer bounded data, batching, event-driven work, and coalescing over polling
- Keep hot and real-time paths deterministic, non-blocking, and allocation-light
- Keep expensive work outside lock scopes and reuse buffers when ownership is clear
- Measure improvements with a stated workload, baseline, and variance
- Prefer behavior-focused tests for boundaries, transitions, errors, and resource limits

## Commits

- Prefer one coherent responsibility per commit, but do not require strict atomicity or independent compilability
- Commit clearly separable mainline changes first; leave deeply interwoven miscellaneous changes for other commits
- State partial commit boundaries and remaining dependencies explicitly
- Use a concise Conventional Commit subject, one concrete change per body line
- Use real newlines, no terminal punctuation, and no blank lines between items
- Prefix every commit body item, match repository history
 
## Storage

- Build outputs and other large temporary artifacts MUST be created under the repository `.cache/` directory
- Agents MUST treat `/tmp` and other memory-backed filesystems as memory resources, avoid placing large artifacts there,
  and check RAM/swap pressure before starting memory-intensive operations
- Agents MUST check available storage before creating large artifacts and keep cache growth bounded
- Agents MUST clean stale cache entries when configured storage or operation thresholds are reached, while preserving active builds and artifacts still in use
