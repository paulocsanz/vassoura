---
name: vassoura
description: >
  Operate the watermarked build-artifact collector (the `vassoura` CLI):
  see disk and eligible bytes, clean with dry-run/--apply, run the
  hysteresis daemon, evict tool classes (ollama/docker/rustup/pnpm/go), and
  read the status directory (~/Vassoura). Use when the user asks to free
  disk, how much space is reclaimable, disk status, turn the daemon on,
  remove old ollama models, /vassoura, or mentions vassoura/ledger/seen.db.
---

# vassoura — watermark collector

The disk never fills up: the least-recently-used regenerable directory
leaves BEFORE space gets tight, and every removal is audited in the ledger
(`~/.vassoura/ledger.jsonl`) and reversible by regeneration.

## Invariants (breaking any of these is a bug)

1. **never-yours**: a candidate exists only inside the config allowlist;
   `.git` is never walked or removed; a symlink is never followed.
2. **nothing-without-ledger**: every removal writes a ledger line with a
   regeneration hint BEFORE success is reported.
3. **dry-by-default**: `clean` / `tools` without `--apply` remove nothing;
   `--apply` without a terminal requires `--yes`.
4. **nothing-in-use**: minimum age per class + a re-stat at removal time +
   use gates (lsof/git). An unavailable gate fails CLOSED (skips).

## Commands (every one is accepted by the real CLI)

```sh
vassoura status                  # disk, watermarks, verdict, eligible now
vassoura status --json           # the same report as JSON
vassoura scan                    # catalog: size + age + class per candidate
vassoura scan --root ~/src/app --top 10
vassoura scan --json
vassoura clean                   # dry-run: LRU plan up to the target (nothing removed)
vassoura clean --apply           # runs the plan (asks on a terminal)
vassoura clean --apply --yes     # no prompt (required if stdin is not a terminal)
vassoura clean --older-than 30   # override the minimum age (days) for every class
vassoura clean --until-free 120  # free-space target in GiB for this plan
vassoura clean --top 20          # maximum items in the plan
vassoura daemon --once           # ONE daemon cycle (hysteresis, seen.db, statusfs) and exit
vassoura daemon                  # continuous loop (poll every watch_interval_secs)
vassoura tools                   # tool-class plan (dry-run)
vassoura tools --apply           # runs each tool's own CLI (asks)
vassoura tools --apply --yes     # same, no prompt
vassoura refresh                 # rewrites the status directory (~/Vassoura, one file per metric)
vassoura install-daemon          # writes the daemon LaunchAgent (loading is up to you)
vassoura --config PATH …         # alternate config (default ~/.vassoura/config.toml)
```

## How to decide

- **"How much space can I get back?"** → `vassoura status` (human) or
  `vassoura status --json` (machine). Verdict: `ok` at or above the high
  mark, `tight` between the marks, `critical` below the low mark.
- **"Clean X"** → ALWAYS dry-run first (`vassoura clean [--root X]`), show
  the plan, and run `--apply` only with an explicit yes. Without a
  terminal, `--apply --yes`.
- **"The disk filled up again / turn the automatic one on"** →
  `vassoura daemon --once` for one audible cycle, and
  `vassoura install-daemon` + `launchctl load …` for the LaunchAgent (your
  decision).
- **"Remove old ollama models / docker prune / old rustup"** →
  `vassoura tools` (the dry-run lists the plan per tool; a missing tool is
  skipped and reported — fail-closed). `--apply` only with a yes.
- **"What has been removed so far?"** → read the ledger
  `~/.vassoura/ledger.jsonl` (one JSON line per removal, with a
  regeneration hint).
- **Watch without running anything** → plain files in `~/Vassoura/`
  (`verdict.txt`, `disk_free_bytes.txt`, `eligible_bytes.txt`, …) — the
  same values as `status --json`, updated by `refresh` / the daemon.

## Config

`~/.vassoura/config.toml` (written on the first run): allowlist roots,
watermarks (`low_watermark_gib` is the trigger / `until_free_gib` is the
target), minimum age per class, `daemon` (per-cycle bound in GiB and items,
rate-limit, notify), `tools` (toggles + `rustup_keep`), paths for the
ledger, seen.db, and the status directory.

## Do not

- Run `clean --apply` / `tools --apply` without showing the dry-run first.
- Touch Downloads, Documents, `.git`, or Docker volumes — outside the
  allowlist by construction.
- Promise exact bytes: the ledger records what was measured; `docker prune`
  reports its own total; pnpm and go do not report one (a line with 0 bytes
  is normal).
