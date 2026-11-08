# Parity with bro-cli

The full per-item checklist (one row per INV item) is Task 42. This file starts with the
decisions taken while porting, so they are recorded where that checklist will live.

## Decisions

- **Codex 5xx is terminal within the Codex pool, after a same-account backoff** (INV §4 "Other
  non-OK ... Codex 5xx is terminal within Codex pool today — decide"). Aligned with Anthropic: a
  Codex 5xx is retried on the same account with the overload backoff (`timeouts.overload_retry_*`,
  full jitter, `retry-after` wins) and, if it never clears, surfaced as `Terminal(Overloaded)` with
  a legible `api_error` message; no other Codex account is tried, and the account is not
  sidelined. The cross-provider hop rule still applies (a 500/503/529 is hop-eligible).
  Tests: `server_error_backs_off_then_terminal`, `pre_commit_error_not_streamed_in_200`.
- **Codex pre-commit holds `message_start`** (changed vs bro): the stream commits on the first
  content frame (or the 64 KiB cap / a forced `message_start`), not on `message_start`, so an
  upstream failure right after `response.created` is settled as `Retry`/`Terminal` instead of a
  200 carrying only `message_start` + `error`. Test: `pre_commit_error_not_streamed_in_200`.
- **Codex error types are mapped to the Anthropic set** (changed vs bro, which forwarded Codex
  codes such as `rate_limit_exceeded` as the error `type`). Test:
  `error_frames_use_anthropic_error_types`.
