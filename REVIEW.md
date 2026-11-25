# REVIEW.md

What a review of a limits-tui change checks beyond the global review rules. The rules themselves live in
`.claude/rules/`; this file says which to check and why each exists. Reviewers read this file and the rule files from
the base branch (`git show origin/<base>:REVIEW.md`, `git show origin/<base>:.claude/rules/<file>`), so a change
never judges itself by rules it edits. Check every Focus rule; check a Checklist item only when its trigger matches the
diff.

## Focus

| Rule | Source |
| --- | --- |
| `probes.md` Bounded | |
| `source-data.md` Untrusted input | |
| `probes.md` Safe working directory | |
| `probes.md` Devin's key over https only | |
| `tui.md` Failed refreshes stay visible | 18c186e F4, 9bc432f |
| `tui.md` Event-loop timing | 670d2a7, b3cfa9a |
| `tests.md` Scratch dirs, Signals | e4caae1 |
| `tests.md` Regression tests | |

## Checklist

Trigger: `paths:` globs match the diff's file names; `diff contains:` is a regex over its added and
removed lines; an item with both needs both. `\|` in this table is the regex `|`. New items need a
diff-readable trigger, a source, and either two occurrences or one miss by review
(`~/.claude/rules/review.md`). Retired IDs are not reused.

| ID | Trigger | Rule | Source |
| --- | --- | --- | --- |
| LT-1 | paths: `src/probe/**`; diff contains: `Command::new\|killpg\|setsid\|O_NONBLOCK\|read_to_end\|waitid` | `probes.md` LT-1 | 18c186e (F2), 784898b, 9bc432f (login-shell lookup, missed by the first review); cd83f9e (Rpc reader), 2026-11-19/20; rust-reviewer memory `subprocess_deadline_patterns` |
| LT-2 | paths: `src/probe/**`; diff contains: `Command::new\|app-server\|current_dir\|"PATH"\|split_paths\|CODEX_BIN\|command -v\|MARKER` | `probes.md` LT-2, for every child this diff spawns | e4caae1, d8fbc3b (PR #4); a1f034a (PR #5), 2026-11-19/20 |
| LT-3 | paths: `src/**`; diff contains: `from_secs_f64\|\+ Duration\|Instant::now\(\) \+\|checked_add\|checked_sub\|saturating_\|clamp\(\|as u64\|parse::<f64>\|_LIMITS_TIMEOUT` | `source-data.md` LT-3 | 670d2a7, e4caae1, 2026-11-19/20 |
| LT-4 | paths: `src/**`; diff contains: `truncate\|is_char_boundary\|floor_char_boundary\|char_indices\|&[a-z_.]+\[\.\.[a-z_]+\]\|escape_\|printable` | `source-data.md` LT-4 | 18c186e (F3), e4caae1, 2026-11-19/20 |
| LT-5 | paths: `src/probe/**`; diff contains: `NotSignedIn\|NoQuota\|auth_hint\|rpc_error\|words\(\|\.get\("` | `probes.md` LT-5 | 670d2a7, fce5505; b3cfa9a (recall lost by 670d2a7, caught by rust-reviewer), 2026-11-19; rust-reviewer memory `keyword_classifier_recall` |
