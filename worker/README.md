# Lunch Tray Worker

Runs the task list on Cloudflare, so it is online when your computer is
not. The Worker keeps the tasks, syncs with GitHub on a timer, and serves
the MCP tools. The desktop app shows the Worker's tasks and sends your
changes to it.

It uses one Durable Object with SQLite storage, which the Workers Free plan
includes. The Worker code is Rust, built to wasm with `workers-rs`. It
shares the task model, the sync, and the MCP handler with the app through
`crates/core`.

Forgejo is not synced from the Worker. Use the app without `[remote]` for
that.

## Set up

You need a Cloudflare account, Node.js for `wrangler`, and the
`wasm32-unknown-unknown` Rust target (`rustup target add
wasm32-unknown-unknown`).

```sh
cd worker
npx wrangler@latest login
```

Set two secrets. `API_TOKEN` is the bearer token for everything the Worker
serves; make it long and random. `GITHUB_TOKEN` must be able to read your
pull requests and issues. A fine-grained token with read access is the
safest choice. `gh auth token` also works, but that token can do much more.

```sh
openssl rand -hex 32 | tee /dev/stderr | npx wrangler@latest secret put API_TOKEN
npx wrangler@latest secret put GITHUB_TOKEN
```

The settings are under `[vars]` in `wrangler.toml`: poll interval, where
closed items go, CI checks, searches, and excluded repositories. Then:

```sh
npx wrangler@latest deploy
```

The output gives the Worker's URL, for example
`https://lunch-tray.<your-subdomain>.workers.dev`.

## Point the app at it

Add this to `~/.config/lunch-tray/config.toml`:

```toml
[remote]
url = "https://lunch-tray.<your-subdomain>.workers.dev"
token = "<API_TOKEN>"
```

Send your current tasks to the Worker once, then restart the app:

```sh
lunch-tray --upload
systemctl --user restart lunch-tray
```

`--upload` only fills an empty Worker. `--upload --replace` overwrites it.

With `[remote]`, the app no longer syncs on its own. It also ignores
`[[github]]`, `[[forgejo]]`, and `[mcp]`. It checks the Worker every 10
seconds (`poll_secs`). It sends a change the moment you make it, and keeps
a copy of the tasks in `~/.local/share/lunch-tray/remote-cache.json`. When
the Worker cannot be reached, the tray shows the error badge and the window
shows the last tasks it had.

## MCP

The endpoint is `https://lunch-tray.<your-subdomain>.workers.dev/mcp`, with
the same tools and protocol as the app's local server:

```sh
claude mcp add --transport http lunch-tray https://lunch-tray.<your-subdomain>.workers.dev/mcp \
  --header "Authorization: Bearer <API_TOKEN>"
```

## Free plan limits

| Limit | Use |
|---|---|
| 100,000 requests a day | One alarm every 2 minutes (720), plus the app's checks every 10 seconds while it runs (8,640), plus MCP calls |
| 100,000 rows written a day | Only tasks that changed, plus one row for the sync status per pass |
| 50 subrequests per run | 3 searches, about one CI request per 15 open pull requests, and at most `MAX_INDIVIDUAL_CHECKS` (20) single fetches |
| 10 ms CPU per run | See below |

The CPU limit is the one to watch. Each pass parses GitHub's answers and
updates the tasks, and a Durable Object that wakes up first reads every
task. If passes fail with "exceeded CPU" in `npx wrangler@latest tail`,
the Workers Paid plan ($5 a month) raises the limit to 30 seconds.

## Develop

```sh
printf 'API_TOKEN=dev-token\nGITHUB_TOKEN=%s\n' "$(gh auth token)" > .dev.vars
npx wrangler@latest dev
```

`wrangler dev` needs a recent wrangler: older ones lack runtime APIs that
`workers-rs` 0.9 calls. To test the app's link against it:

```sh
LUNCH_TRAY_TEST_WORKER=http://127.0.0.1:8787 LUNCH_TRAY_REMOTE_TOKEN=dev-token \
  cargo test -- --ignored
```
