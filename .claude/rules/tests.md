---
paths:
  - "src/**"
---

# Test rules

Each rule's history is in `REVIEW.md`.

- **Scratch dirs.** Tests make each scratch dir fresh (mode 0700, random suffix) and never reuse a fixed `/tmp` path
  another user can plant.
- **Signals.** A test never signals pid ≤ 0.
- **Regression tests.** Each regression test fails on the commit before the fix (check it against a scratch copy of the
  base).
