---
paths:
  - "src/probe/**"
---

# Probe rules

Each rule's history is in `REVIEW.md`.

- **Bounded.** Every child process and HTTP call has a deadline, and nothing a probe spawns, reads or holds open
  outlives it.
- **Safe working directory.** A child process starts where a planted file in an untrusted checkout can't be run:
  working dir `/`, no empty or relative PATH entries.
- **Devin's key over https only.** Devin's API key goes over https only, redirects included.
- **LT-1 Waiting on children.** A deadline-bounded child is waited on by its exit, not stdout EOF. Its output is read
  non-blocking on the probing thread, capped in bytes (lines too), and keeps the tail. After the grace the whole group
  gets SIGKILL, and the read end closes on return, so a `setsid`'d holder can't pin a thread or a pipe. Test with a
  fake shell: an in-group background job, a `setsid` holder, a flooding writer, silence.
- **LT-2 Where children run.** Every spawned child sets `current_dir("/")`, and a relative `CODEX_BIN` is made
  absolute before use. The login-shell lookup trusts only what follows its marker line; no marker means no binary.
- **LT-5 Classifying replies.** Each reply lands in the right state. A malformed or unrecognised reply is `Failed`,
  never "not signed in"; a valid reply without usage windows is `NoQuota`; an absent or null optional field is not
  an error (Connect JSON omits zeros). A narrowed keyword classifier still accepts its old positives, inflections and
  camelCase codes.
