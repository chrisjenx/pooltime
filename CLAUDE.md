# CLAUDE.md

pooltime is a from-scratch Rust replacement for bro-cli's LLM subscription pool (`~/git/personal/bro-cli`). It pools
Claude Max and ChatGPT/Codex accounts behind one local Anthropic-compatible endpoint for Claude Code.

## Start here
1. `docs/session-2026-11-07.md` — why this exists, what the user decided, live findings.
2. `docs/specs/2026-11-07-pooltime-design.md` — the spec. **§16 overrides earlier sections.**
3. `docs/specs/2026-11-07-pooltime-parity-inventory.md` — every bro behavior that must be ported or consciously dropped.
4. `docs/plans/2026-11-07-pooltime.md` — the task-by-task plan. Execute with superpowers:subagent-driven-development.

## Rules
- Repo `github.com/chrisjenx/pooltime` (public). Never open PRs against `JustSuperHuman/bro-cli`; bro-fork PRs use
  `--repo chrisjenx/bro-cli`.
- Never print, log, or commit tokens/credentials. Never modify bro's `~/.claude-max-pool`, `~/.bro`, its Keychain
  items, or the `~/.claude-work` profile. Ask before changing the live `~/.claude` / `~/.claude-personal` profiles.
- Tests: `cargo test`; no network in tests. Gates before every commit: `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- The plan's Global Constraints apply to every task.
