# dengai

A tiny stand-in for [ArchiSteamFarm](https://github.com/JustArchiNET/ArchiSteamFarm) (ASF) for **one**
Steam account, built to run all the time on a 1 GB board such as the Orange Pi Zero 3. One static
binary, one process per account, no plugins, no web UI.

What it does:

- **Login** through Steam's `IAuthenticationService` flow (password + Steam Guard code from your
  mobile authenticator's `shared_secret`, a one-off code, or approval in the Steam app). The refresh
  token is saved in the data dir and reused, so the password is only needed once.
- **Card farming** like ASF's simple mode: reads your badge pages, plays one game with drops left at
  a time, re-checks its drops every 5 minutes, moves on at 0, stops playing when nothing is left and
  re-scans every few hours. Pauses while you play on another device (Steam's playing-session
  state) and resumes afterwards.
- **Key redeem** (`CMsgClientRegisterKey`) with readable results, a delay between keys, and a stop on
  rate limit.
- **Free licenses**: claim `sub/<id>` (store web endpoint) and `a/<id>` (`CMsgClientRequestFreeLicense`)
  on request, and **auto-claim** free packages and apps as they appear in PICS (plus an optional
  backfill list on the first start), with the filter rules of the
  [FreePackages](https://github.com/Citrinate/FreePackages) ASF plugin, a token-bucket limit
  (30/hour by default) and a persistent queue.
- **Steam time sync** for Steam Guard codes (`ITwoFactorService/QueryTime`), so a board with a
  drifting clock still logs in.
- A small **HTTP API** (bearer token, 127.0.0.1 by default) for status, redeeming keys, adding
  licenses and pausing farming.

## Credit

dengai is a small re-implementation of ideas from **ArchiSteamFarm** by Łukasz "JustArchi" Domeradzki
and contributors, and from the **FreePackages** plugin by Citrinate. Parts of the logic (badge page
parsing, free-license result handling, the free-package filter) are ported from them under the
Apache License 2.0; see [NOTICE](NOTICE) and [LICENSE-APACHE](LICENSE-APACHE). If you want the full
feature set (many bots, trading, plugins, IPC GUI), use ASF. The Steam network layer is the
[steam-vent](https://codeberg.org/steam-vent/steam-vent) crate.

## Steam rules and risks

- Use it for **your own single account**. Idling, redeeming and claiming free licenses are things
  the official client does too, but automation is at your own risk; Valve may change or limit any
  of it.
- Steam rate-limits key activations (roughly 10 failed keys per hour) and free licenses (about 50
  per hour). dengai stops a `/redeem` batch on `RateLimited`, and on a free-license limit waits an hour,
  then 2, 4 and 8 hours on each repeat until a claim succeeds (kept across restarts). It claims at most
  `claims_per_hour` licenses per hour, `/addlicense` included, and rejects a queued license after 5
  failed attempts.
- Farming plays a game on your account: while dengai farms, friends see you "in game". When you
  start a game on another device, Steam blocks playing on dengai's session and farming pauses.
- Keep the data dir private: `refresh_token` is a long-lived login for your account.

## Quick start

```sh
export DENGAI_USERNAME=myaccount
export DENGAI_PASSWORD='...'                  # first login only
export DENGAI_SHARED_SECRET='base64secret='   # optional: from your authenticator's maFile
export DENGAI_API_TOKEN="$(head -c 24 /dev/urandom | base64)"
./dengai
```

Without `DENGAI_SHARED_SECRET`, dengai waits for you to approve the login in the Steam mobile app, or
reads a code from the terminal when started interactively, or uses `DENGAI_GUARD_CODE` (email code).
After the first login only `DENGAI_USERNAME` is needed; remove the password if you like.

## First login on the setup page

A Steam Guard code expires within a minute, so a deploy can't carry one. Instead deploy with only
`DENGAI_API_TOKEN` (at least 12 characters) and sign in on the page dengai serves at `/`:

1. Reach the API from your machine: `ssh -L 8737:127.0.0.1:8737 <board>`, then open
   http://127.0.0.1:8737/. Or over Tailscale: set `DENGAI_API_BIND` to the board's tailnet IP and port (for example `100.64.0.5:8737`) and open that address.
2. Paste the API token (kept in the tab's session storage), then enter username, password and, if you
   like, a Steam Guard code or your authenticator's `shared_secret`. Otherwise approve the sign-in in
   the Steam app, or type the code the page asks for.

With no saved refresh token and no `DENGAI_PASSWORD`, dengai waits in `needs_login` and logs
`open http://<api_bind>/ to sign in`; it doesn't exit. Without `DENGAI_API_TOKEN` there is no page, so
dengai logs an error and idles.

The username is saved next to the refresh token. The password is never saved or logged. A
`shared_secret` entered on the page is saved in the data dir (mode 0600), and later sign-ins fill the
Steam Guard code from it. When Steam rejects the refresh token (it expires after a few months) dengai
goes back to `needs_login` and you sign in again. `/login` accepts one attempt per 10 seconds.

Setting `DENGAI_USERNAME` and `DENGAI_PASSWORD` (with `DENGAI_SHARED_SECRET` or `DENGAI_GUARD_CODE`) in
the environment still works for headless setups, exactly as below.

## Configuration

Every option can be set in a TOML file (`DENGAI_CONFIG`, default `./dengai.toml`, optional) with
the key name shown, or with an environment variable `DENGAI_<KEY>` (upper case), which wins.

| Key | Default | Meaning |
| --- | --- | --- |
| `username` | none | Steam account name; with a password it is required, otherwise the setup page asks for it. |
| `password` | none | Password, only for a headless first login; leave unset to sign in on the setup page. |
| `shared_secret` | none | Base64 `shared_secret` of the mobile authenticator; generates Steam Guard codes (on Steam's clock, re-synced every 12 h; local clock if the sync fails). |
| `guard_code` | none | One-off Steam Guard code (email or app) for the first login. |
| `data_dir` | `data` (`/data` in Docker) | Holds `refresh_token`, `guard.json` and `state.json`. |
| `log` | `info` | `error`, `warn`, `info`, `debug` or `trace`. |
| `api_bind` | `127.0.0.1:8737` | API listen address. |
| `api_token` | empty | Bearer token for the API, at least 12 characters (startup fails on a shorter one). The API is off when empty. |
| `farm` | `true` | Farm card drops. |
| `farm_check_secs` | `300` | Seconds between drop checks of the game being played (min 60). |
| `farm_rescan_secs` | `21600` | Seconds between badge scans when nothing is left to farm. |
| `redeem_delay_secs` | `5` | Seconds between keys / licenses in `/redeem` and `/addlicense`. |
| `auto_claim` | `true` | Watch PICS and claim free packages (and apps, see below). |
| `auto_claim_apps` | `true` | Also auto-claim free apps (`a/<id>`), not only packages. |
| `pics_poll_secs` | `60` | Seconds between PICS change polls (min 30). |
| `claims_per_hour` | `30` | Token-bucket size for automatic claims. |
| `skip_demos` | `false` | Don't auto-claim packages containing demos. |
| `skip_playtests` | `false` | Don't auto-claim packages containing playtests. |
| `skip_unowned_dlc` | `true` | Don't auto-claim DLC for games you don't own. |
| `backfill_url` | empty (off) | URL of a list of free `sub/<id>` / `a/<id>` to check and queue, see [First start](#first-start-and-backfill). |
| `backfill_interval_secs` | `0` | Seconds between backfills; `0` fetches the list only once. |
| `country` | login IP country | Country used for region rules. |

Booleans accept `true/false`, `1/0`, `yes/no`, `on/off`. Example `dengai.toml`:

```toml
username = "myaccount"
shared_secret = "base64secret="
api_token = "change-me-to-a-long-random-token"
skip_demos = true
claims_per_hour = 20
```

### Auto-claim rules

A changed package is queued when it is free (`billingtype` 10 NoCost or 12 FreeOnDemand), has
`status` 0 and a single-purchase license, its `starttime`/`expirytime` window is open, it isn't a
deactivated demo or a playtest package, you don't own it (license list), you don't own its
`dontgrantifappidowned` app, you own its `mustownapptopurchase` app, the region rules
(`restrictedcountries`, `purchaserestrictedcountries`) allow your country, and its apps are
released (not checked for NoCost), not all owned already, not region-locked, and not skipped by
`skip_*`.

A changed app (`auto_claim_apps`) is queued as `a/<id>` and claimed with
`CMsgClientRequestFreeLicense` when it is free (`extended/isfreeapp`, or `common/type` demo),
released (`releasestate` released/preloadonly or `state` eStateAvailable), not owned, its
`mustownapptopurchase` app is owned, `restricted_countries` / `purchaserestrictedcountries` allow
your country, and it isn't skipped by `skip_demos` / `skip_unowned_dlc`. Playtests (type beta) are
never claimed as apps: FreePackages joins them through the store's playtest request, which dengai
doesn't implement. When Steam answers OK but grants nothing (usually region-locked), the app is
marked rejected and not tried again.

The queue (`sub/<id>` and `a/<id>` entries), the done/rejected sets, the last PICS change number
and the backfill time are kept in `data_dir/state.json` (written atomically).

### First start and backfill

PICS only reports changes, so on the first start dengai records the current change number and
evaluates packages and apps from then on; free licenses that already exist are never seen. To
catch up, set `backfill_url` to a list of known free licenses. dengai fetches it once (when
`state.json` has no `backfilled_at` yet), then every `backfill_interval_secs` if that is above 0.
Every id from the list goes through the same PICS product-info check and filter as a PICS change,
in batches, before it is queued, so a wrong or hostile list can't make it claim paid items; ids
already done, rejected, queued or owned are dropped. Claims still go through the token bucket.

The list is plain text, one `sub/123` (or `s/123`) or `a/456` per line, `#` comments and blank lines
ignored, or a JSON array of the same strings. One maintained public list in this format is the
"Steam Codes" gist used by [ASFclaim](https://github.com/C4illin/ASFclaim) (about 1900 entries,
updated daily); or use your own list:

```toml
backfill_url = "https://gist.githubusercontent.com/C4illin/e8c5cf365d816f2640242bf01d8d3675/raw/Steam%20Codes"
backfill_interval_secs = 86400
```

## HTTP API

`GET /` (the setup page) is open and holds no secrets. All other endpoints need `Authorization: Bearer $DENGAI_API_TOKEN`. `/redeem` and `/addlicense` take 1 to 50
items per request and answer 503 if Steam hasn't finished within 120 s (the batch keeps running; see
`/status`).

```sh
API=http://127.0.0.1:8737; AUTH="Authorization: Bearer $DENGAI_API_TOKEN"

# farming game, drops left, claim queue size, recent claims
curl -H "$AUTH" $API/status

# redeem keys (stops at the first RateLimited)
curl -H "$AUTH" -d '{"keys":["AAAAA-BBBBB-CCCCC"]}' $API/redeem

# free licenses: sub/<id> (store) or a/<id> (free app)
curl -H "$AUTH" -d '{"ids":["sub/123","a/456"]}' $API/addlicense

curl -H "$AUTH" -X POST $API/farm/pause
curl -H "$AUTH" -X POST $API/farm/resume
```

The setup page uses `POST /login` (`{"username","password","guard_code"?,"shared_secret"?}`, answers 202,
400 for a bad body, 409 unless `/status` says `needs_login`, 429 within 10 s of the last attempt) and
`POST /login/code` (`{"code"}`, only while `login` is `waiting_for_code`). `/status` reports `login`
(`connecting`, `needs_login`, `waiting_for_code`, `waiting_for_app_approval` or `logged_in`), `username`
and `login_error`.

`/status` returns for example:

```json
{"login":"logged_in","username":"myaccount","login_error":null,
 "connected":true,"steam_id":76561198000000000,
 "farming":{"appid":440,"name":"Team Fortress 2","drops":3},"games_to_farm":4,
 "farm_paused":false,"playing_blocked":false,"claim_queue":2,"change_number":31234567,
 "recent_claims":[{"id":"sub/123","result":"ok","at":1767225600}]}
```

## Docker (Orange Pi Zero 3)

Multi-arch images (`linux/arm64`, `linux/amd64`) are published to Docker Hub as `sudtanj/dengai`
by [infrastructure-tools/builder-tools/dengai](https://github.com/sudtanj/infrastructure-tools/tree/main/builder-tools/dengai)
(distroless, user 65532, `/data` volume, compose files with a 64 MB limit there).

```sh
docker volume create dengai
docker run -d --name dengai --restart unless-stopped -p 127.0.0.1:8737:8737 \
  --memory 64m \
  -e DENGAI_USERNAME=myaccount \
  -e DENGAI_PASSWORD='...' \
  -e DENGAI_SHARED_SECRET='base64secret=' \
  -e DENGAI_API_TOKEN=change-me-to-a-long-random-token \
  -v dengai:/data \
  sudtanj/dengai:latest
docker logs -f dengai
```

The image binds the API on `0.0.0.0:8737` inside the container; `-p 127.0.0.1:8737:8737` keeps it on
the board's loopback. Don't combine the image with `--network host` unless you also set
`-e DENGAI_API_BIND=127.0.0.1:8737`. Without a shared secret, run the first login with
`docker run -it ...` to type the Steam Guard code, or approve it in the app.

Binaries for `aarch64-unknown-linux-musl` and `x86_64-unknown-linux-musl` are attached to each
`v*` release and to every CI run.

## Resource use

Target: under 30 MB resident memory and near-zero CPU while idling, so it fits next to other
containers on a 1 GB board. It runs on a single-threaded Tokio runtime, keeps only package/app ids
in memory, and parses PICS data batch by batch. The release build uses `opt-level = "s"`, fat LTO,
one codegen unit, `panic = "abort"` and stripping; there is no OpenSSL anywhere (rustls).

## Build

```sh
cargo build --release
cross build --release --target aarch64-unknown-linux-musl   # static binary for the Orange Pi
cargo test
```

## License

PolyForm Noncommercial 1.0.0 for dengai's own code, Apache-2.0 for the parts ported from
ArchiSteamFarm and FreePackages. See [LICENSE](LICENSE), [NOTICE](NOTICE),
[LICENSE-POLYFORM-NONCOMMERCIAL.md](LICENSE-POLYFORM-NONCOMMERCIAL.md) and
[LICENSE-APACHE](LICENSE-APACHE).
