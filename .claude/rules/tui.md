---
paths:
  - "src/main.rs"
  - "src/ui.rs"
---

# TUI rules

Each rule's history is in `REVIEW.md`.

- **Failed refreshes stay visible.** A failed refresh is visible beside the cached data it leaves on screen: its label
  (else "update failed") right after the source name, inside 80 columns with two live windows and the plan. Staleness
  alone never hides an error.
- **Event-loop timing.** A running probe neither spins the event loop at its 10 ms floor nor delays its own result past
  `PROBE_CHECK`: a new probe path goes through `start_probe`, or moves `next_at` ahead itself as the Claude path
  does.
