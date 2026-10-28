# AGENTS.md — vassoura

Watermarked build-artifact collector (Rust CLI + daemon).

## Invariants (breaking any of these is a bug)

1. **never-yours**: a candidate exists only inside the config allowlist;
   `.git` is never walked or removed; a symlink is never followed.
   Worktree exception (2026-09-30): a git worktree checkout listed via a
   `worktree_repos` repo IS collectible as `class = worktree` — its `.git`
   is a pointer file, not a repository — but only after the merged gate
   (HEAD is an ancestor of the origin default branch, unknown origin =
   fail-closed skip) and removal goes through `git worktree remove` without
   `--force`, which refuses dirty or locked checkouts. The main checkout,
   bare, locked and prunable entries are never candidates.
2. **nothing-without-ledger**: every removal writes a ledger line with a
   regeneration hint before reporting success.
3. **dry-by-default**: `clean` without `--apply` removes nothing; `--apply`
   without a terminal requires `--yes`.
4. **nothing-in-use**: minimum age per class + a re-stat at removal time
   (mtime diverged from the scan → skip). The tight-mode minimum has a hard
   1h floor (`MIN_AGE_FLOOR_DAYS`) — no config value, tight disk included,
   ever makes an in-flight build evictable.
   Emergency exception (2026-10-28 incident): on critical disk usage (>= 98% / forced cycle / emergency floor),
   the minimum age floor for pure regenerable artifacts (`Class::Artifact`) drops to 10 minutes (`EMERGENCY_MIN_AGE_FLOOR_DAYS`).
   In-flight builds remain strictly protected by `lsof` gating and re-stat mtime checks, while finished builds older than 10 minutes
   become collectible to prevent ENOSPC host lockup. Worktrees never drop below the hard 1h floor (`MIN_AGE_FLOOR_DAYS`).
5. **no-churn** (2026-09 incident, 2026-10 emergency fix): the daemon never re-evicts a path
   removed in the last 7 days (`CHURN_GUARD_DAYS`), and when consecutive
   eviction cycles buy no durable free space the churn breaker suspends
   evictions with exponential backoff (1h → 24h) and says so — deleting
   faster than things regenerate is a machine-killer, not cleanup.
   Emergency rule (2026-10-06 incident): an external write storm (steep plunge in free space)
   is never classified as regeneration churn, and on critical disk usage (>= 95% / forced cycle)
   OS survival takes absolute priority over churn backoff: the daemon evicts eligible
   candidates rather than allowing the disk to reach 0 bytes (ENOSPC).
6. **scan-degraded-honesty** (2026-10-01 incident): when the scan cannot
   measure (workers killed by memory/IO pressure), the daemon degrades to
   the shallow PROBE scan, says so (log + macOS notification) and retries
   the full scan with exponential cycle backoff. It never reports a false
   "0 candidates · IDLE" — an empty catalog under pressure is a degraded
   state, not a clean bill of health.
7. **darwin-var-folders-leaf-isolation** (2026-10 incident): macOS `/var/folders` (and `/private/var/folders`)
   has a multi-tier layout: `<bucket>/<user_hash>/[T,C,X,0]/<candidate>`.
   Candidates are NEVER cataloged at bucket or user-hash level (root-owned,
   multi-tenant, in perpetual use by daemons). Enumeration traverses to depth 4
   so each discrete directory (Playwright artifacts, test dirs, browser caches)
   is an independent candidate, preventing wedge/timeout on 90,000 files in one job.
8. **nested-app-cache-leaf-isolation** (2026-10 incident): monolithic cache trees whose
   root directories are perpetually touched by live apps (e.g. Chrome code sign clones
   or Claude Desktop scratchpads in `/tmp/claude-*`) are expanded to discrete leaf session
   candidates. This ensures dead sessions/clones are individually collectible without
   parent mtime freshness starving cleanup.
9. **candidate-gate-windowing** (2026-10 incident): daemon cycles evaluate use gates
   (lsof and git status) only on a bounded top-scoring window (up to 3x `cap_items`).
   Evaluating gates against thousands of candidates causes lsof timeouts and git contention
   under I/O pressure. On emergency disk pressure, lsof timeouts do not block regenerable
   artifact eviction.
10. **monorepo-subproject-promotion** (2026-10 incident): monorepo or container
   workspaces with subproject build artifacts (e.g. `forja/engine/target`,
   `janela/escritorio/target`, or subprojects inside non-git container dirs)
   promote shallow artifacts down to depth 3 to `ScanJob::Measure` immediately.
   This ensures multi-gigabyte subproject build artifacts run in the first 500ms
   of the scan rather than starving behind thousands of app cache items in `Hunt`.
11. **transient-search-and-virtiofs-gate-isolation** (2026-10 incident): transient
   read-only searches (`grep`, `rg`, `find`, `mdworker`) and Linux guest VFS
   inode caches via Apple Virtualization framework / VirtioFS (`com.apple.Virtualization.*`)
   are isolated from active host compiler/build processes. Read-only search tools
   never block artifact eviction, and in emergency disk pressure (>= 98% / forced cycle),
   idle VirtioFS guest handles do not block regenerable artifact eviction.

## Working rules

- `cargo test && cargo clippy -- -D warnings` are green before every land.
