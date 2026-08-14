# BrainDrain

BrainDrain shows coding-agent subscription usage in its CLI and desktop
frontends.

## Web frontend

The web frontend embeds the same Rust provider backend as the macOS app and
serves a small server-rendered usage dashboard. It refreshes provider state at
startup and every five minutes, with a manual refresh action in the page. Run it
with:

```console
mise run web
```

It listens on `127.0.0.1:8080` by default. Set `BRAINDRAIN_WEB_LISTEN` to a
different socket address when a trusted reverse proxy needs to reach it. The web
service does not implement browser authentication or credential management;
deployments must put it behind an authentication proxy.

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
