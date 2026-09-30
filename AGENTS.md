# AGENTS.md — vassoura

Watermarked build-artifact collector (Rust CLI + daemon).

## Invariants (breaking any of these is a bug)

1. **never-yours**: a candidate exists only inside the config allowlist;
   `.git` is never walked or removed; a symlink is never followed.
2. **nothing-without-ledger**: every removal writes a ledger line with a
   regeneration hint before reporting success.
3. **dry-by-default**: `clean` without `--apply` removes nothing; `--apply`
   without a terminal requires `--yes`.
4. **nothing-in-use**: minimum age per class + a re-stat at removal time
   (mtime diverged from the scan → skip). The tight-mode minimum has a hard
   1h floor (`MIN_AGE_FLOOR_DAYS`) — no config value, tight disk included,
   ever makes an in-flight build evictable.
5. **no-churn** (2026-09 incident): the daemon never re-evicts a path
   removed in the last 7 days (`CHURN_GUARD_DAYS`), and when consecutive
   eviction cycles buy no durable free space the churn breaker suspends
   evictions with exponential backoff (1h → 24h) and says so — deleting
   faster than things regenerate is a machine-killer, not cleanup.

## Working rules

- `cargo test && cargo clippy -- -D warnings` are green before every land.
