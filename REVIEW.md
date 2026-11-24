# REVIEW.md

What a review of a limits-tui change checks beyond the global review rules. Reviewers read this file
from the base branch (`git show origin/<base>:REVIEW.md`), so a change never judges itself by
rules it edits. Check every Focus line; check a Checklist item only when its trigger matches the
diff.

## Focus

- Every probe is bounded: each child process and HTTP call has a deadline, and nothing a probe spawns, reads or holds open outlives it.
- What Claude Code, Codex and Devin return (files, JSON, error text, env values) is untrusted: no value can panic the TUI or `--once`, and control characters never reach the terminal raw.
- A child process starts where a planted file in an untrusted checkout can't be run: working dir `/`, no empty or relative PATH entries.
- Devin's API key goes over https only, redirects included.
- A failed refresh is visible beside the cached data it leaves on screen: its label (else "update failed") right after the source name, inside 80 columns with two live windows and the plan. Staleness alone never hides an error (18c186e F4, 9bc432f).
- A running probe neither spins the event loop at its 10 ms floor nor delays its own result past `PROBE_CHECK`: a new probe path goes through `start_probe`, or moves `next_at` ahead itself as the Claude path does (670d2a7, b3cfa9a).
- Tests make each scratch dir fresh (mode 0700, random suffix) and never reuse a fixed `/tmp` path another user can plant; a test never signals pid ≤ 0 (e4caae1).
- Each regression test fails on the commit before the fix (check it against a scratch copy of the base).

## Checklist

Trigger: `paths:` globs match the diff's file names; `diff contains:` is a regex over its added and
removed lines; an item with both needs both. `\|` in this table is the regex `|`. New items need a
diff-readable trigger, a source, and either two occurrences or one miss by review
(`~/.claude/rules/review.md`). Retired IDs are not reused.

| ID | Trigger | Check | Source |
|---|---|---|---|
| LT-1 | paths: `src/probe/**`; diff contains: `Command::new\|killpg\|setsid\|O_NONBLOCK\|read_to_end\|waitid` | A deadline-bounded child is waited on by its exit, not stdout EOF. Its output is read non-blocking on the probing thread, capped in bytes (lines too), and keeps the tail. After the grace the whole group gets SIGKILL, and the read end closes on return, so a `setsid`'d holder can't pin a thread or a pipe. Test with a fake shell: an in-group background job, a `setsid` holder, a flooding writer, silence. | 18c186e (F2), 784898b, 9bc432f (login-shell lookup, missed by the first review); cd83f9e (Rpc reader), 2026-11-19/20; rust-reviewer memory `subprocess_deadline_patterns` |
| LT-2 | paths: `src/probe/**`; diff contains: `Command::new\|app-server\|current_dir\|"PATH"\|split_paths\|CODEX_BIN\|command -v\|MARKER` | Every child this diff spawns sets `current_dir("/")`, and a relative `CODEX_BIN` is made absolute before use. The login-shell lookup trusts only what follows its marker line; no marker means no binary. | e4caae1, d8fbc3b (PR #4); a1f034a (PR #5), 2026-11-19/20 |
| LT-3 | paths: `src/**`; diff contains: `from_secs_f64\|\+ Duration\|Instant::now\(\) \+\|checked_add\|checked_sub\|saturating_\|clamp\(\|as u64\|parse::<f64>\|_LIMITS_TIMEOUT` | Arithmetic on values from a source or the environment saturates or clamps. NaN, inf and huge `*_LIMITS_TIMEOUT` values, extreme timestamps and out-of-range percentages must not panic `Duration::from_secs_f64`, `Instant + Duration` or a debug build's overflow check. | 670d2a7, e4caae1, 2026-11-19/20 |
| LT-4 | paths: `src/**`; diff contains: `truncate\|is_char_boundary\|floor_char_boundary\|char_indices\|&[a-z_.]+\[\.\.[a-z_]+\]\|escape_\|printable` | Source text (plan names, error details, server messages) is cut at a char boundary (`floor_char_boundary`), never a byte index. Every field `--once` prints goes through `printable`. | 18c186e (F3), e4caae1, 2026-11-19/20 |
| LT-5 | paths: `src/probe/**`; diff contains: `NotSignedIn\|NoQuota\|auth_hint\|rpc_error\|words\(\|\.get\("` | Each reply lands in the right state. A malformed or unrecognised reply is `Failed`, never "not signed in"; a valid reply without usage windows is `NoQuota`; an absent or null optional field is not an error (Connect JSON omits zeros). A narrowed keyword classifier still accepts its old positives, inflections and camelCase codes. | 670d2a7, fce5505; b3cfa9a (recall lost by 670d2a7, caught by rust-reviewer), 2026-11-19; rust-reviewer memory `keyword_classifier_recall` |
