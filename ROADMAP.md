# Roadmap

## Claude max-extraction: warm-up period

The tokens-per-percent join needs at least 2 window instances with at
least 10% observed growth in the current regime. Claude accrues window
instances only after `merma install` wires the statusline hook. Until
then the report says "not yet" instead of guessing. Candidate work:
backfill utilization from the OAuth poller history once enough polls
accumulate, and state the confidence separately per source.

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
