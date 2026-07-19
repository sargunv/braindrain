# BrainDrain

BrainDrain shows coding-agent subscription usage in its CLI and desktop
frontends.

## Kimi Coding Plan

Install and sign in with the first-party
[Kimi Code CLI](https://github.com/MoonshotAI/kimi-cli), then run:

```console
braindrain check kimi
```

BrainDrain reads the OAuth credentials that Kimi Code stores at
`~/.kimi/credentials/kimi-code.json` (or under `KIMI_SHARE_DIR`), refreshes an
expiring token using the same cross-process lock and OAuth protocol, and fetches
subscription usage from `https://api.kimi.com/coding/v1/usages`. It preserves
unknown credential fields when Kimi rotates tokens. `KIMI_CODE_BASE_URL` can
override the API base for testing or compatible deployments; `KIMI_API_KEY` is
used only when no Kimi Code credential file exists.

Usage is subscription-wide and does not depend on a selected inference model, so
BrainDrain does not hard-code or rewrite Kimi's dynamic model catalog.

The implementation tracks the first-party protocol and storage layout at
MoonshotAI/kimi-cli commit
[`4a550eff`](https://github.com/MoonshotAI/kimi-cli/tree/4a550effdfcb29a25a5d325bf935296cc50cd417).
