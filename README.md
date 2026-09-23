# vassoura

Evicts regenerable build artifacts before the disk fills. The least-recently-used directory goes first. Every removal is written to an append-only ledger with the command that brings it back.

Nothing is deleted unless you pass `--apply`. Outside a terminal, `--apply` also requires `--yes`.

## Usage

```sh
vassoura status                         # free space, watermarks, what is eligible
vassoura status --json
vassoura scan --top 15                  # size, age, and class
vassoura clean                          # dry-run
vassoura clean --apply                  # asks first
vassoura clean --apply --yes

vassoura tools                          # ollama, docker, rustup, pnpm, go (dry-run)
vassoura tools --apply --yes

vassoura daemon --once                  # one cycle
vassoura install-daemon                 # writes a LaunchAgent; you load it
vassoura refresh                        # writes ~/Vassoura, one file per metric
```

`~/.vassoura/config.toml` is created on the first run. It lists the roots vassoura is allowed to touch, the low watermark (default 40 GiB, that is when a cycle starts deleting) and the free-space target (default 100 GiB). The ledger is `~/.vassoura/ledger.jsonl`.

A cycle skips a directory that holds an open file, a git worktree with uncommitted work, or a `.git` directory. A missing `lsof` or `git` skips the candidate instead of deleting it. Docker prune never includes volumes.

## Build

```sh
cargo test && cargo clippy -- -D warnings
cargo build --release
```
