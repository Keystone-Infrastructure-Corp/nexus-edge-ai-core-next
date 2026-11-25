# Review Rules

Canonical copy: `nexus-home/ci/REVIEW.md`. Every repo carries a verbatim copy.

Every review that can run the `requesting-code-review` skill does, with this file as the
fresh reviewer's checklist: implement-loop's final review on every change, and any
`@claude` request for a review other than `@claude review`. P0 and P1 are the skill's
Critical and Important; its Minor tier is not reported. Copilot code review, Codex and
Claude Code Review cannot run a skill, so they apply this file directly.

## What to report

Report only problems that would ship a wrong result, open a security hole, or lose
data. Mark each one:

- **P0**: breaks production, leaks data across tenants or to the public, loses or
  corrupts data, or exposes a secret.
- **P1**: a correctness or security bug a user or attacker can reach, or a broken
  AGENTS.md rule.

Give `file:line` evidence for every finding, and say what goes wrong and when. If you
cannot point at a line, do not post the finding.

## The checklist

The rules in the repo's AGENTS.md, including its `## Code Review Rules` section, are
the checklist. Check each changed file against every rule that applies to it. A broken
rule is at least P1.

Also flag:

- New non-trivial logic (a branch, a loop, a parser, auth, money, data migration)
  that no test exercises.
- A rendered assertion (an empty state, an all-clear, a count, a verdict) derived from
  a defaulted value such as `data ?? []` or `x?.n ?? 0` rather than from the query's
  loading and error state. "Still loading" and "failed" must not read as a real answer.

## What not to report

- Style, formatting, naming or import order. CI enforces those.
- Generated files: lockfiles, codegen output (OpenAPI, protobuf, API types), build
  output, snapshots, vendored code and knowledge-graph exports.
- Suggestions, refactors and "consider" comments that fix no bug.
- Anything already raised on the PR.

## Close every review with

- The findings, or "No P0 or P1 findings."
- What you could not check: tests or gates you could not run, files too large to
  read, and behaviour that depends on runtime state, hardware or another repo.
