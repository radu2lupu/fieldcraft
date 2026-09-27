# Security

## Reporting a vulnerability

Please report security issues privately through GitHub's
[private vulnerability reporting](https://github.com/radu2lupu/fieldcraft/security/advisories/new)
rather than a public issue. Include steps to reproduce and the version or commit you tested.
You should get a response within a week.

## What to keep in mind when running Fieldcraft

- **Lessons are derived from your agent transcripts.** The distiller reads Claude Code, Codex and
  Grok session logs and writes summaries into `~/.fieldcraft/memory.sqlite3`. Anything an agent saw
  (file contents, command output, pasted secrets) can end up in a lesson. Treat the store with the
  same care as the transcripts themselves.
- **Distillation and embeddings call external APIs.** Lesson text is sent to the configured
  embedding provider and, for distillation, to the configured agent CLI or model API. Do not run
  Fieldcraft on work you are not allowed to send to those providers.
- **API keys are stored in `~/.fieldcraft/secrets.env`** (owner-only, plain text, read by the
  binaries when the environment lacks a key). Keep that file out of backups you share, and rotate
  the key there rather than in launchd plists or shell profiles.
- **The server is meant for a private network.** `fieldcraft-server` listens on all interfaces
  and authenticates every data route with a bearer token. Tokens are minted by `POST /enroll`,
  which by default only accepts loopback callers. To enroll other machines, set
  `FIELDCRAFT_ENROLL_SECRET` on the server and pass it to `install.sh --enroll-secret`; once a
  secret is set, every caller (loopback included) must present it, so a reverse proxy in front
  of the server cannot turn remote callers into trusted local ones. `FIELDCRAFT_ENROLL_OPEN=1`
  restores unauthenticated enrollment and is only appropriate on a network you fully trust.
  Do not expose the server to the public internet.
- **Retrieved lessons are untrusted input to the agent.** A lesson is text written by an earlier
  session. Agents should treat it as context, not as instructions that override the user.
