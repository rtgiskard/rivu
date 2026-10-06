# Agent Constraints

## Design

- Prefer elegant, boring, KISS solutions over clever abstractions
- Keep boundaries, ownership, failure states, and control flow explicit
- Add abstractions only to remove real duplication or isolate real change
- Reuse existing conventions and prefer clean cutovers; remove obsolete paths rather than adding compatibility debt or transitional shims
- Protect correctness, readability, visual consistency, and six-month maintainability
- State the invariant, cost, and tradeoff behind non-obvious choices

## Efficiency

- Treat allocation, copying, locking, syscalls, wakeups, and unbounded growth as costs
- Prefer bounded data, batching, event-driven work, and coalescing over polling
- Keep hot and real-time paths deterministic, non-blocking, and allocation-light
- Keep expensive work outside lock scopes and reuse buffers when ownership is clear
- Demonstrate improvements with a stated workload and baseline; report repeated measurements and variability for timing comparisons, or counts and bytes for deterministic allocation or capacity changes

## Tests

- Keep permanent tests for plausible consumer-visible bugs, boundaries, transitions, errors, and resource limits
- Avoid tests of implementation details, wording, incidental defaults, and obsolete compatibility behavior
- Delete obsolete tests; merge redundant tests without losing distinct behavioral coverage
- Do not pursue coverage numbers; weigh bug-prevention value against setup, runtime, and maintenance costs, and use throwaway smoke checks for one-off verification

## Commits

- Prefer one coherent responsibility per commit without requiring strict atomicity
- Commit clearly separable mainline changes first; leave deeply interwoven miscellaneous changes for other commits
- Default to buildable commits; when phased cutovers require dependent commits, state boundaries, dependencies, and the validated end state explicitly
- Use a concise Conventional Commit subject, one concrete change per body line
- Use real newlines, no terminal punctuation, and no blank lines between items
- Prefix every commit body item, match repository history

## Storage

- Build outputs and other large temporary artifacts MUST be created under the repository `.cache/` directory
- Agents MUST treat `/tmp` and other memory-backed filesystems as memory resources, avoid placing large artifacts there,
  and check RAM/swap pressure before starting memory-intensive operations
- Agents MUST check available storage before creating large artifacts and keep cache growth bounded
- Agents MUST limit cleanup to confirmed task-owned temporary artifacts that are no longer in use
- Agents MUST preserve active builds and artifacts and MUST NOT delete cache entries with uncertain ownership or active status
- Agents MUST pause large artifact generation when storage is insufficient until safe cleanup or additional storage resolves it
