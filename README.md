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

On Unix, set `BRAINDRAIN_WEB_UNIX_SOCKET` to serve through a Unix-domain socket
instead of TCP. The parent directory should be private to the service and its
reverse proxy. Set `BRAINDRAIN_WEB_ORIGIN` to the externally visible origin in
reverse-proxy deployments; refresh POSTs without that exact `Origin` are
rejected.

## Kimi Coding Plan

Install and sign in with the current first-party
[Kimi Code CLI](https://github.com/MoonshotAI/kimi-code) or the legacy
[Kimi CLI](https://github.com/MoonshotAI/kimi-cli), then run:

```console
braindrain check kimi
```

BrainDrain reads the OAuth credentials that current Kimi Code stores at
`~/.kimi-code/credentials/kimi-code.json` (or under `KIMI_CODE_HOME`). It falls
back to the legacy Kimi CLI location at `~/.kimi/credentials/kimi-code.json` (or
under `KIMI_SHARE_DIR`). BrainDrain refreshes an expiring token using the same
cross-process lock and OAuth protocol, and fetches subscription usage from
`https://api.kimi.com/coding/v1/usages`. It preserves unknown credential fields
when Kimi rotates tokens. `KIMI_CODE_BASE_URL` can override the API base for
testing or compatible deployments; `KIMI_API_KEY` is used only when no Kimi Code
credential file exists.

Usage is subscription-wide and does not depend on a selected inference model, so
BrainDrain does not hard-code or rewrite Kimi's dynamic model catalog.

The OAuth implementation tracks the first-party protocol at MoonshotAI/kimi-cli
commit
[`4a550eff`](https://github.com/MoonshotAI/kimi-cli/tree/4a550effdfcb29a25a5d325bf935296cc50cd417).

## Google AI / Gemini / Antigravity

Sign in using the Antigravity CLI (`agy`), then run:

```console
braindrain check google
```

(Aliases `gemini`, `google-ai`, `antigravity`, and `agy` are also supported.)

BrainDrain automatically discovers credentials saved by the Antigravity CLI in
the system keyring (service `gemini`, account `antigravity`). It automatically
refreshes expiring OAuth access tokens, retrieves account details and plan tier
via Antigravity's `daily-cloudcode-pa.googleapis.com` backend, and tracks the
weekly and five-hour quota windows for Gemini and Claude/GPT models. The generic
`cloudcode-pa.googleapis.com` backend can return unused buckets even when the
Antigravity quota is exhausted. Refreshed tokens are cached in-process without
writing to the CLI's keyring. Missing or invalid quota measurements are omitted;
a response with no usable measurements is reported as an error.

You can also provide or override credentials via environment variables:
`GOOGLE_AI_ACCESS_TOKEN` (or `GEMINI_ACCESS_TOKEN`), `GOOGLE_AI_REFRESH_TOKEN`,
`GOOGLE_AI_PROJECT_ID` (or `GOOGLE_CLOUD_PROJECT`), and `GOOGLE_AI_BASE_URL`.
