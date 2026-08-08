# Roadmap

## The INSUFFICIENT tier ships the cold start

Calibration needs 4 qualifying instances (≥10 pt observed growth, all
admission gates passed) in the current regime. Until then the brief
shows the INSUFFICIENT tier: exactly what is missing and the earliest
unlock date, re-evaluated every scan — `now + ceil(k/q) × regime` at
the observed qualifying rate, or 4 regimes from the first snapshot when
no complete window has been observed. Claude accrues instances only
after `merma install` wires the statusline hook. Candidate work:
backfill utilization from the OAuth poller history once enough polls
accumulate, and state the confidence separately per source.

## Estimator candidates (evidence-gated)

- Adopt the inner `[x₂,xₙ₋₁]` band below n = 8 if
  `MAX_INSTANCES_FOR_ESTIMATE` ever grows.
- Revisit drift modeling only if the calibration set grows past ~15
  instances; below that, recency + the wide exact band is the
  defensible treatment (see DESIGN.md rejected alternatives).
- A watch screen exists only if real `watch merma` usage demonstrates a
  need for sub-window texture; `watch -n 300 merma` composes the live
  view for free from plain piped bytes.

## Billing-overlap selection: coverage tail

The greedy longest-window-first selection excludes overlapping
instances wholesale. The non-overlapping tail of a shorter instance is
dropped, which undercounts *coverage*. The report shows the uncovered
time as UNKNOWN, so the error is conservative. Candidate work: split
partially-overlapping instances at the overlap boundary instead of
dropping them.

## Rewrite detection: hash window

The rewrite detector hashes the first 256 bytes of a file and verifies
the newline at the resume boundary. A rewrite located after byte 256
that preserves the size ordering and the boundary newline passes
undetected. Session logs are not adversarial, so the risk is accepted.
Candidate work: a rolling checksum over the scanned prefix.

## Fast-mode and surcharge visibility

Fast-mode multipliers and web-search surcharges do not appear in local
logs. API-equivalent figures are floors when those features are in use.
Candidate work: detect fast-mode markers in transcripts if they appear
in a future schema.

## Linux collection

`merma install --launchd` is macOS-only. Linux users can run
`merma collect --quiet` from cron or a systemd timer. Candidate work:
`merma install --systemd` writing a user unit + timer.

## Distribution

- Publish the crate to crates.io. The package metadata is ready.
