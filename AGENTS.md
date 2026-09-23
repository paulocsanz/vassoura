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
   (mtime diverged from the scan → skip).

## Working rules

- `cargo test && cargo clippy -- -D warnings` are green before every land.
