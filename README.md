# BrainDrain

BrainDrain shows coding-agent subscription usage in its CLI, desktop apps, and
web frontend.

## Providers

| Provider                                                            | ID            | Aliases                                     |
| ------------------------------------------------------------------- | ------------- | ------------------------------------------- |
| [OpenAI / Codex](#openai--codex)                                    | `openai`      | `codex`                                     |
| [Claude](#claude)                                                   | `claude`      | `claude-code`, `anthropic`                  |
| [Cursor](#cursor)                                                   | `cursor`      | None                                        |
| [Kimi Coding Plan](#kimi-coding-plan)                               | `kimi`        | `kimi-code`, `kimi-coding-plan`             |
| [Z.ai Coding Plan](#zai-coding-plan)                                | `zai`         | `z.ai`                                      |
| [OpenCode Go](#opencode-go)                                         | `opencode-go` | `opencode`, `zen-go`, `opencode-zen`        |
| [Google AI / Gemini / Antigravity](#google-ai--gemini--antigravity) | `google`      | `google-ai`, `gemini`, `antigravity`, `agy` |

Run `braindrain providers` to list provider IDs. Use
`braindrain info <provider>` to inspect local credential discovery and
`braindrain check <provider>` to fetch usage. From a source checkout, use
`mise run cli -- <command>`.

### OpenAI / Codex

BrainDrain reads Codex OAuth credentials from `~/.codex/auth.json`, or
`$CODEX_HOME/auth.json` when `CODEX_HOME` is set. Sign in to Codex with your
ChatGPT account and ensure its credentials are stored in that file, then run:

```console
braindrain check openai
```

BrainDrain fetches ChatGPT subscription usage and reset credits. It refreshes
expiring OAuth tokens and writes the updated credentials back to `auth.json`.

### Claude

Sign in with Claude Code, then run:

```console
braindrain check claude
```

BrainDrain reads `~/.claude/.credentials.json`, or `.credentials.json` under
`CLAUDE_CONFIG_DIR`. If the file is absent, it checks the macOS Keychain entry
`Claude Code-credentials`. `CLAUDE_CODE_OAUTH_TOKEN` takes precedence over both.

File-based OAuth credentials can be refreshed and written back. Keychain
credentials are read-only, and directly supplied tokens are not refreshed. If
those tokens expire, sign in with Claude Code again or replace the supplied
token.

### Cursor

Sign in with the Cursor CLI, then run:

```console
braindrain check cursor
```

BrainDrain reads `~/.config/cursor/auth.json`, or
`$XDG_CONFIG_HOME/cursor/auth.json` when `XDG_CONFIG_HOME` is set. If no token
is found there, it checks `CURSOR_AUTH_TOKEN`, then the system keyring entry
with service `cursor-access-token` and account `cursor-user`.

The provider fetches current billing-period usage and plan details. It does not
refresh OAuth tokens; renew an expired token through Cursor.

### Kimi Coding Plan

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

### Z.ai Coding Plan

BrainDrain discovers Z.ai API keys saved by OpenCode in
`~/.local/share/opencode/auth.json`, or `$XDG_DATA_HOME/opencode/auth.json` when
`XDG_DATA_HOME` is set. It accepts API-key entries named `zai-coding-plan`,
`zai`, or `z.ai`. If no key is found, it uses `Z_AI_API_KEY`.

With a Coding Plan key configured, run:

```console
braindrain check zai
```

The default quota host is `https://api.z.ai`. Set `Z_AI_API_HOST` to use another
host, such as `https://open.bigmodel.cn`, or set `Z_AI_QUOTA_URL` to override
the full quota endpoint.

### OpenCode Go

Sign in to the OpenCode website, then store your workspace URL or ID and auth
cookie through BrainDrain's interactive prompt:

```console
braindrain auth login opencode-go
braindrain check opencode-go
```

BrainDrain stores these credentials in the system keyring. Alternatively, set
both `OPENCODE_WORKSPACE_ID` and `OPENCODE_AUTH_COOKIE`; together they take
precedence over the stored credentials. The provider reads usage limits from the
workspace's Go page. An OpenCode model API key cannot replace the website auth
cookie.

Use `braindrain auth logout opencode-go` to remove the stored credentials.

### Google AI / Gemini / Antigravity

Sign in using the Antigravity CLI (`agy`), then run:

```console
braindrain check google
```

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
