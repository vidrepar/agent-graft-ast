# agent-graft-ast — rules for every agent and subagent here

- Never `rm` with an unguarded variable path. Write `rm -f "${DB:?}"/*.redb`, or use a literal path inside `bench/` or a `mktemp -d` dir you just created.
- pnpm only for any JS tooling. `cargo audit` and `cargo test` must be clean before every commit.
- You own engineering decisions (merges, refactors, benchmark design). Escalate only money, destructive actions, or scope changes.
- Benchmarks are measured, never estimated. Anything unmeasurable is marked "not measured".
