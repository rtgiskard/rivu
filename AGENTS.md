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

- Keep each commit atomic and limited to one coherent responsibility
- Use a concise Conventional Commit subject
- Write one concrete change per body line
- Separate subject and body with one blank line
- Use real newlines, no terminal punctuation, and no blank lines between items
