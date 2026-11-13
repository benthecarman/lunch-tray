# Lunch Tray Worker

Runs the task list on Cloudflare, so it is online when your computer is
not. The Worker keeps the tasks, syncs with GitHub on a timer, and serves
the MCP tools. The desktop app shows the Worker's tasks and sends your
changes to it.

It uses one Durable Object with SQLite storage, which the Workers Free plan
includes. The Worker code is Rust, built to wasm with `workers-rs`. It
shares the task model, the sync, and the MCP handler with the app through
`crates/core`.

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
pull requests and issues:

- If every repository you follow is public, or they all belong to one
  owner, a fine-grained token with read-only access is the safest choice.
- A fine-grained token covers one owner. For private repositories under
  several owners, use a [classic token with only `repo` and
  `read:org`](https://github.com/settings/tokens/new?scopes=repo,read:org&description=lunch-tray%20worker).
- `gh auth token` also works, but that token can do much more, and
  `gh auth logout` revokes it.

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

To start from tasks you already have, such as a `tasks.json` from an older
version, load them into the new Worker, then restart the app:

```sh
lunch-tray --upload ~/.local/share/lunch-tray/tasks.json
systemctl --user restart lunch-tray
```

`--upload` only fills an empty Worker. `--upload FILE --replace` overwrites
it.

The app sends a change the moment you make it, and keeps a copy of the
tasks in `~/.local/share/lunch-tray/remote-cache.json`. When the Worker
cannot be reached, the tray shows the error badge and the window shows the
last tasks it had.

The app downloads the whole list once at start, then opens a WebSocket.
The Worker pushes each change down it the moment it happens: only the tasks
that changed, never the whole list. The socket hibernates on Cloudflare's
side, so while nothing happens it costs nothing, and the runtime answers
the app's pings without waking the Worker.

When the socket cannot be opened or drops, the app catches up over HTTP
and checks every 10 seconds while a window is open (`poll_secs`) and every
30 seconds with only the tray icon (`idle_poll_secs`), asking only for what
changed. It tries the socket again after 5 seconds, then waits twice as
long each time, up to 5 minutes.

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
| 100,000 requests a day | One alarm every 2 minutes (720), one per socket the app opens, its own changes, and MCP calls. Polling, only without a socket, adds 2,880 to 8,640 |
| 100,000 rows written a day | Only tasks that changed, plus a few rows per pass for the sync status and the revision |
| 5,000,000 rows read a day | A few per pushed change; the whole list (one row per task) only for each sync pass, op, and MCP call |
| 50 subrequests per run | 3 searches, about one CI request per 15 open pull requests, and at most `MAX_INDIVIDUAL_CHECKS` (20) single fetches |
| 10 ms CPU per run | See below |

The CPU limit is the one to watch. Each pass parses GitHub's answers and
updates the tasks, and first reads every task, since the Durable Object
sleeps between requests. A check for changes reads no tasks. If passes
fail with "exceeded CPU" in `npx wrangler@latest tail`, the Workers Paid
plan ($5 a month) raises the limit to 30 seconds.

## Develop

```sh
printf 'API_TOKEN=dev-token\nGITHUB_TOKEN=%s\n' "$(gh auth token)" > .dev.vars
npx wrangler@latest dev
```

`wrangler dev` needs a recent wrangler: older ones lack runtime APIs that
`workers-rs` 0.9 calls. To test the app's link against it, one test at a
time:

```sh
LUNCH_TRAY_TEST_WORKER=http://127.0.0.1:8787 LUNCH_TRAY_REMOTE_TOKEN=dev-token \
  cargo test -- --ignored --test-threads=1
```

CI does the same on every push; see `.github/workflows/ci.yml`.
