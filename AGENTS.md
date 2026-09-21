# Instructions for coding agents

This file tells a coding agent how to install, verify, and modify this
project safely. If you are a human, read [README.md](README.md).

## Task: install merma for the user

### Preconditions — verify, do not assume

1. Run `cargo --version`. Require 1.88 or later. If Rust is missing, stop
   and tell the user to install it from https://rustup.rs.
2. Run `git --version`. Require any version.
3. Confirm `~/.claude/settings.json` parses as JSON if it exists. If it
   does not parse, STOP. Report the parse error to the user. Do not edit a
   broken file.

### Steps — idempotent, safe to re-run

1. `cargo build --release`
2. `install -d ~/.local/bin && install -m 755 target/release/merma ~/.local/bin/merma`
3. Run `~/.local/bin/merma install --print`. Show the user what would
   change. Do not edit `~/.claude/settings.json` yourself — the binary
   does it, keeps a backup, and prints the rollback path.
4. Run `~/.local/bin/merma install`. On macOS, add `--launchd` if the
   user wants background collection every 15 minutes.
5. If the install output warns about transcript retention, offer
   `merma install --fix-retention`. It raises `cleanupPeriodDays` to 365
   so history stops evaporating.
6. Run `~/.local/bin/merma scan` for the first ingest.

### Postconditions — verify before you report success

1. `~/.local/bin/merma --version` exits 0.
2. `echo '{}' | ~/.local/bin/merma statusline-hook` exits 0.
3. `~/.local/bin/merma doctor` shows `✓ store` and `✓ claude transcripts`.
4. `python3 -c "import json; json.load(open('$HOME/.claude/settings.json'))"`
   exits 0.

### Report to the user

- The backup path that `merma install` printed, so the user can roll
  back.
- The one-line rollback instruction: `merma uninstall` restores the
  previous statusline command.
- Whether the Claude OAuth poller is active, and that
  `claude_oauth_enabled = false` in `~/.merma/config.toml` turns it off.

### Prohibitions

- Do not use sudo.
- Do not edit `~/.claude/settings.json` directly. Use `merma install`
  and `merma uninstall`.
- Do not delete or rename an existing statusline script. merma chains
  to it and it is the rollback.
- Do not schedule `merma collect` more often than the launchd
  default (15 minutes). Each run polls the live endpoints once.

## Task: contribute a change

1. Read [agm.json](agm.json). Compute the risk zone: for each changed
   file, take the highest-severity zone whose pattern matches it; the
   change's zone is the highest across all files.
2. Prepare the evidence package in the pull-request body with the
   sections agm.json requires for that zone. Start from
   `.github/PULL_REQUEST_TEMPLATE.md`.
3. State every external assumption (transcript schema, rollout schema,
   `rate_limits` variants, settings keys, endpoint shapes, prices) and
   how you verified it against real data. Declare what you could not
   verify. An unverified assumption stated as fact is a governance
   failure, not a shortcut.
4. For high and critical zones: STOP before you submit. Show the human
   the diff and the package. Ask the human to check the confirmation
   box. Do not check it yourself.
5. Never claim maintainer approval. The `AGM` check passing is not
   approval; the maintainer's review is.
6. Disclose your tool and model in the PR body. Keep the
   `Co-Authored-By` trailer on commits.

## Task: modify the code

- Obey the rules in [DESIGN.md](DESIGN.md).
- Run `cargo test` and `cargo clippy --all-targets -- -D warnings` before
  you report done. CI enforces both, plus `cargo fmt --check`.
- Every estimate must scale by the measured span (interval union), never
  by the requested period.
- Every new warning or note must reach every read path: brief, status,
  doctor.
- Never hardcode a billing-window duration. Reconstruct windows from
  snapshot data.
- New parser beliefs need a fixture test from real data. Fixtures live
  in `tests/fixtures/`.
- `--json` output is a contract. Renderers may change; serialized fields
  may only grow WITHIN a schema_version. The 0.2.0 reset (breaking from
  the 0.1.x report shape) is documented in README §`--json`.

## Code Review Rules

Review as the person who runs this software, not as a style checker. A
finding names an input, a state or a sequence of actions that produces a
wrong result, lost data or a broken flow for that person, and says what
they see when it happens. Without that, there is no finding.

### A confident wrong answer is the worst outcome

A visible failure is better than a quiet lie. Flag anything that turns a
missing or malformed input into a plausible-looking result: a fallback
that invents a value, a partial result presented as complete, an error
swallowed into a default, a retry that hides the first failure.

Safe path: fail loudly, return nothing, or mark the result as degraded
where the user will see it.

### Data that crosses a boundary is not ours

Input from a network, a file, a user, a model or another process can be
absent, malformed or hostile. Flag a read that does not cover those
cases, and any secret, token or personal datum that reaches a log, a URL,
an error message or a third party.

Safe path: validate at the boundary, keep secrets out of anything that
is stored or shared, and declare what could not be verified.

### What not to report

CI owns formatting, lint and tests. Do not spend a comment on
formatting, naming, comment wording, test names, doc phrasing, or a
refactor that changes nothing the user sees. Do not report an input the
system cannot receive, or a defect someone has to construct on purpose
to trigger.

One finding that costs a user beats five that cost a reviewer their
attention. When nothing meets that bar, say so and approve.
