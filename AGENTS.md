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
- When claiming performance gains, provide a stated workload and baseline; report repeated measurements and variability for timing comparisons, or counts and bytes for deterministic allocation or capacity changes; ordinary internal cleanup does not automatically require benchmarks

## Development Workflow

- MUST limit changes to the authorized scope and preserve user and other work; do not revert, clean up, or include unrelated changes in commits without authorization, and report unrelated findings separately
- Validate the integrated change before delivery, not after every edit; choose checks according to the affected behavior and build paths
- For Rust source changes, MUST run `make lint` and relevant existing behavior tests before delivery; if no suitable test exists, use a throwaway behavior check, and add permanent tests only when justified by the criteria in Tests
- `make lint` uses all features, including font embedding; see [Makefile](Makefile) for check definitions and [readme.adoc](readme.adoc) for build and font prerequisites
- For build configuration, dependency, linking, or release-path changes, MUST run `make build`
- When source or build changes affect conditional compilation or feature behavior, MUST validate the affected enabled and disabled feature combinations; `--all-features` does not cover disabled-feature branches, and unaffected combinations need not be checked
- For user-visible behavior changes, MUST run the program and exercise the changed path; compilation and automated tests alone do not prove runtime behavior
- For documentation-only changes, check content, links, and command consistency; lint and builds are not required unless the change also affects executable code or build behavior
- For parallel work, run shared full checks once after integration rather than duplicating them in each agent; each agent still performs the local reproduction and behavior checks needed for its changes
- Reuse successful verification when the relevant code, configuration, and environment are unchanged; rerun only checks whose evidence has become invalid
- MUST NOT suppress warnings, weaken assertions, or change unrelated code merely to make checks pass
- If verification is blocked, report the exact command, blocking prerequisite or failure, and unverified scope; a narrower check MUST NOT be presented as the full check passing
- At delivery, report the verification actually performed and its results, including any remaining blockers

## Tests

- Keep permanent tests for plausible consumer-visible bugs, boundaries, transitions, errors, and resource limits
- Avoid tests of implementation details, wording, incidental defaults, and obsolete compatibility behavior
- Delete obsolete tests; merge redundant tests without losing distinct behavioral coverage
- Do not pursue coverage numbers; weigh bug-prevention value against setup, runtime, and maintenance costs, and use throwaway smoke checks for one-off verification
- Permanent tests MUST be deterministic, isolated, and safe to run independently and with the full suite; make environment dependencies explicit and do not rely on existing developer data or incidental timing

## Documentation

- Keep entry-point documentation concise: project purpose, capabilities, installation, startup, critical limitations, and topical links
- User references describe operations, configuration, results, and errors, not decorative appearance or internal mechanisms; include control names and locations only when needed for a task
- Technical design documents explain protocols, ownership, invariants, resource boundaries, rationale, and alternatives; include code details only when essential to a consequential design decision, not as a code walkthrough
- Update documentation for meaningful behavior, contract, or design changes; pure refactoring, internal optimization, and visual polish do not automatically require updates
- Update the authoritative section and link rather than duplicate; documentation is not a change log or UI acceptance record

## Commits

- Prefer one coherent responsibility per commit without requiring strict atomicity
- Commit clearly separable mainline changes first; leave deeply interwoven miscellaneous changes for other commits
- Default to buildable commits for independent changes; a single logical change MAY span dependent stages without requiring every intermediate commit to be independently buildable or verifiable
- For dependent staged commits, MUST state boundaries and dependencies and validate the integrated final state; MUST NOT add compatibility scaffolding or run extra checks solely to make intermediate commits independently verifiable
- Use a concise Conventional Commit subject, one concrete change per body line
- Use real newlines, no terminal punctuation, and no blank lines between items
- Prefix every commit body item with `- `, matching repository history

## Storage

- Build outputs and other large temporary artifacts MUST be created under the repository `.cache/` directory
- Agents MUST treat `/tmp` and other memory-backed filesystems as memory resources, avoid placing large artifacts there,
  and check RAM/swap pressure before starting memory-intensive operations
- Agents MUST check available storage before creating large artifacts and keep cache growth bounded
- Agents MUST limit cleanup to confirmed task-owned temporary artifacts that are no longer in use
- Agents MUST preserve active builds and artifacts and MUST NOT delete cache entries with uncertain ownership or active status
- Agents MUST pause large artifact generation when storage is insufficient until safe cleanup or additional storage resolves it
