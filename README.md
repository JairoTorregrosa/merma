# merma

**The AI-subscription waste meter.** How much of your Claude Code and Codex
subscriptions did you actually extract — and how much did you leave on the table?

merma reads the usage history both CLIs already write to disk, prices every
token at dated API rates, reconstructs your rate-limit windows from official
utilization snapshots, and tells you in dollars:

```
╭──────────────────────────────────────────╮
│ MERMA WRAPPED · all history              │
│                                          │
│   extracted (API-equivalent)   $3,298.23 │
│   output-only floor            $425.24   │
│   subscriptions cost           $386.87   │
│   multiplier                   8.5×      │
│   left on the table (est.)  ≥ $681.06    │
╰──────────────────────────────────────────╯
```

One Rust binary. Local-only: nothing leaves your machine.

## Quick start

```sh
cargo install --path . --root ~/.local
merma install --launchd   # statusline collector + background scan every 15 min
merma doctor              # verify every data source end to end
merma                     # live dashboard (Ratatui)
merma report --period 30d # retrospective report
merma wrapped --period all
```

`merma install` chains your existing statusline command — your statusline keeps
working exactly as before; merma just taps the JSON feed Claude Code already
sends it. `merma uninstall` restores everything from a timestamped backup.

## Commands

| command | what it does |
|---|---|
| `merma` | live dashboard: overview, per-provider, 52-week heatmap |
| `merma report` | retrospective waste report (`--period 7d/30d/90d/365d/all/Nd`, `--provider`, `--cache full/output-only`, `--json`) |
| `merma wrapped` | shareable recap card |
| `merma scan` | incremental ingest of local history |
| `merma collect` | scan + live polls (what the launchd agent runs) |
| `merma doctor` | diagnose every source, with live cross-checks |
| `merma status` | one-line current utilization for scripts/statuslines |

## How the numbers are made

**Extracted (API-equivalent $)** — every request in your local transcripts and
rollouts, priced at what the same tokens would have cost on the API, using
**dated** price tables (`prices.toml`; models are matched by longest prefix and
by era, so a July request is priced at July prices). Two variants are always
shown, because cache reads are where subscription value concentrates:

- **full** — input + cache writes (5m/1h split when known) + cache reads + output
- **output-only** — the conservative floor

**Left on the table** — from official `used_percent` snapshots (the same
numbers `/usage` shows) merma reconstructs individual rate-limit window
instances, joins credit/token-weighted usage to percent-consumed per instance,
and extrapolates what a 100%-utilization window would be worth. This join is
the one genuinely modeled number, and merma is honest about it:

- calibration is **per window instance** (consecutive-pair regression fails —
  `used_percent` is integer-quantized; verified locally, see VERIFICATION.md)
- the estimate is shown as a **P25–P75 range** with its dispersion, and marked
  UNSTABLE when P75/P25 > 1.5×
- your **achieved best week** is a hard floor: if the join's median lands below
  something you have already done, the bound is reported as "≥ your best", not
  as a fake range
- window identity, count and duration come from the data, never from constants
  (Codex switched from 300-minute to weekly-only windows on 2026-07-12; banked
  resets make "weekly" instances 2–4 days — merma just follows the resets)

**Subscription waste** — official utilization percentages weighted over covered
plan-time: `covered plan $ × (1 − weighted peak utilization)`. Coverage is
always printed; waste outside measured windows is reported as UNKNOWN, not zero.

Unpriced models, external pass-through models (e.g. `moonshotai/…` routed via
Codex), and era-approximate prices are **loudly excluded or flagged**, never
silently folded in.

## Data sources

| source | what | how |
|---|---|---|
| Claude transcripts | per-request tokens | `~/.claude/projects/**/*.jsonl`, deduped by `message.id + requestId` (57% of entries are duplicates) |
| Claude statusline feed | official utilization snapshots | merma's hook taps the statusline JSON and chains to your original command |
| Claude OAuth endpoint | utilization when no session is open | `api.anthropic.com/api/oauth/usage` — see ToS note below, off with one config line |
| Codex rollouts | per-request tokens + embedded rate limits | `~/.codex/sessions/**/*.jsonl` (cumulative counters diffed; archived duplicates and inherited subagent baselines guarded) |
| Codex wham endpoint | live utilization + credits | `chatgpt.com/backend-api/wham/usage` with your own CLI token |

Everything lands in one SQLite database (`~/.merma/merma.db`, WAL). Scans are
incremental (byte-offset resume); a full first scan of 1.3 GB takes ~4 s,
rescans ~0.1 s.

## ToS disclosure — read this

Two of the five sources are **unofficial endpoints** used with your own local
credentials:

- The **Claude OAuth usage endpoint** is not a public API. Anthropic has taken
  action against third-party tools using subscription auth (Feb 2026). merma
  polls it gently (~3 min cadence, only while collecting) and reads your token
  from the macOS Keychain locally — but the sanctioned path is the statusline
  feed + transcripts, which cover everything once the hook is installed. Disable
  the OAuth poller entirely with `claude_oauth_enabled = false` in
  `~/.merma/config.toml`.
- The **wham usage endpoint** is the same call the Codex CLI itself makes; merma
  reuses the CLI's own auth.json token, read locally.

Neither endpoint receives any of your content — merma only asks "how full are
my windows". Everything else is local file reading.

## Configuration

`~/.merma/config.toml` (all optional):

```toml
chain_statusline = "/path/to/your/original/statusline.sh"  # set by `merma install`
claude_plan_id = "default_claude_max_20x"  # else read from ~/.claude.json
claude_monthly_usd = 200.0                 # override plan price
codex_monthly_usd = 20.0
claude_oauth_enabled = false               # kill the OAuth poller
oauth_poll_secs = 180
wham_poll_secs = 180
```

Price tables: the embedded `prices.toml` can be replaced (full replace, no
merge) at `~/.merma/prices.toml`. Entries carry `from`/`until` dates and an
`approx` flag that propagates into every report that uses them.

merma follows a **loud-failure** policy: a missing plan tier, an unreadable
config, or an unparseable price table is a startup error with a remedy, never a
silent default.

## Development

```sh
cargo test      # 16 unit + fixture tests (both Codex eras, dedup, resume, pricing edges)
cargo clippy    # clean
```

See `VERIFICATION.md` for the empirical verification run against real local
data that this implementation was built on.
