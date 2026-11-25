---
paths:
  - "src/**"
---

# Source data rules

Each rule's history is in `REVIEW.md`.

- **Untrusted input.** What Claude Code, Codex and Devin return (files, JSON, error text, env values) is untrusted: no
  value can panic the TUI or `--once`, and control characters never reach the terminal raw.
- **LT-3 Arithmetic.** Arithmetic on values from a source or the environment saturates or clamps. NaN, inf and huge
  `*_LIMITS_TIMEOUT` values, extreme timestamps and out-of-range percentages must not panic
  `Duration::from_secs_f64`, `Instant + Duration` or a debug build's overflow check.
- **LT-4 Text.** Source text (plan names, error details, server messages) is cut at a char boundary
  (`floor_char_boundary`), never a byte index. Every field `--once` prints goes through `printable`.
