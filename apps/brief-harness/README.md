# brief-harness

Cited intelligence briefs, produced unattended on ZeroClaw agents.

The harness watches the public sources named in a vertical pack. When a
source changes, it has three ZeroClaw agents triage the change, write a brief
in which every claim quotes the changed text, and review that brief with a
model from a different vendor. A brief that fails any check is held, never
sent. Passing briefs go out immediately (`autonomous` subscriptions) or wait
for an operator (`approval` subscriptions).

Design, business case, and rollout plan: [`AI_PROJECT_SPEC.md`](../../AI_PROJECT_SPEC.md).

## What keeps it safe to leave running

| Control | Where |
|---|---|
| Models have no tools; source text reaches them only inside an untrusted-data frame | `samples/zeroclaw-agents.toml`, `src/prompts.rs` |
| Fetches limited to the pack's HTTPS URLs; redirects refused; body size capped | `src/pack.rs`, `src/snapshot.rs` |
| No change, no model call | `src/pipeline.rs` |
| Each claim must quote, verbatim, changed text from a snapshot captured in this run | `src/verify.rs` |
| Cross-vendor review of every item before delivery | `src/pipeline.rs`, `src/config.rs` |
| Monthly cap per model, read from ZeroClaw's own cost ledger; unknown spend denies | `src/budget.rs` |
| Kill switch: while `<state>/KILL` exists nothing runs or sends | `src/pipeline.rs` |
| Hash-chained audit log, verified before every append | `src/audit.rs` |

## Setup

1. Merge `samples/zeroclaw-agents.toml` into the ZeroClaw config, bind the
   three provider aliases, add `[cost.rates]` for the three model ids, and
   reload the daemon.
2. Make sure the cost ledger exists. On a daemon that has not recorded any
   cost yet, create it empty: `touch <workspace>/state/costs.jsonl`.
3. Copy `samples/` somewhere outside the repository, replace the placeholder
   sources, and point `cost_ledger` at the ledger.
4. Export the gateway pairing token under the name in `token_env`.
5. `brief-harness --config brief-harness.toml validate`
6. Run once to record baselines (no model spend):
   `brief-harness --config brief-harness.toml run`
7. Schedule `run` (launchd, systemd timer, or cron).

## Commands

All output is JSON.

| Command | Does |
|---|---|
| `validate` | Load the config and packs and cross-check them |
| `run [--subscription ID]` | Run one subscription, or all |
| `budget` | Month-to-date spend, cap, and headroom per model |
| `approvals list` | Pending briefs with their Markdown |
| `approvals approve ID --by NAME` | Send a pending brief to the subscription's current destination |
| `approvals reject ID --by NAME [--reason TEXT]` | Discard a pending brief |
| `audit-verify` | Verify the audit log's hash chain |

## State layout

```text
<state_dir>/
  audit.jsonl                 hash-chained audit log
  snapshots/blobs/..          normalized source text, by SHA-256
  snapshots/latest/<sub>/..   each subscription's last-seen snapshot per source
  approvals/{pending,approved,rejected}/
  held/<run_id>.json          briefs that failed a gate, with reasons
  run.lock                    present while a run is active
  KILL                        create to stop everything
```

## Tests

```bash
cargo test -p brief-harness
cargo clippy -p brief-harness --all-targets -- -D warnings
```
