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
  on request, and **auto-claim** free packages as they appear in PICS, with the filter rules of the
  [FreePackages](https://github.com/Citrinate/FreePackages) ASF plugin, a token-bucket limit
  (30/hour by default) and a persistent queue.
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
  per hour). dengai stops a `/redeem` batch on `RateLimited`, waits an hour on the free-license
  limit, and claims at most `claims_per_hour` packages per hour.
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

## Configuration

Every option can be set in a TOML file (`DENGAI_CONFIG`, default `./dengai.toml`, optional) with
the key name shown, or with an environment variable `DENGAI_<KEY>` (upper case), which wins.

| Key | Default | Meaning |
| --- | --- | --- |
| `username` | (required) | Steam account name. |
| `password` | none | Password, needed only while there is no saved refresh token. |
| `shared_secret` | none | Base64 `shared_secret` of the mobile authenticator; generates Steam Guard codes. |
| `guard_code` | none | One-off Steam Guard code (email or app) for the first login. |
| `data_dir` | `data` (`/data` in Docker) | Holds `refresh_token`, `guard.json` and `state.json`. |
| `log` | `info` | `error`, `warn`, `info`, `debug` or `trace`. |
| `api_bind` | `127.0.0.1:8737` | API listen address. |
| `api_token` | empty | Bearer token for the API. The API is off when empty. |
| `farm` | `true` | Farm card drops. |
| `farm_check_secs` | `300` | Seconds between drop checks of the game being played (min 60). |
| `farm_rescan_secs` | `21600` | Seconds between badge scans when nothing is left to farm. |
| `redeem_delay_secs` | `5` | Seconds between keys / licenses in `/redeem` and `/addlicense`. |
| `auto_claim` | `true` | Watch PICS and claim free packages. |
| `pics_poll_secs` | `60` | Seconds between PICS change polls (min 30). |
| `claims_per_hour` | `30` | Token-bucket size for automatic claims. |
| `skip_demos` | `false` | Don't auto-claim packages containing demos. |
| `skip_playtests` | `false` | Don't auto-claim packages containing playtests. |
| `skip_unowned_dlc` | `true` | Don't auto-claim DLC for games you don't own. |
| `country` | login IP country | Country used for region rules. |

Booleans accept `true/false`, `1/0`, `yes/no`, `on/off`. Example `dengai.toml`:

```toml
username = "myaccount"
shared_secret = "base64secret="
api_token = "change-me"
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
`skip_*`. The queue, the done/rejected sets and the last PICS change number are kept in
`data_dir/state.json` (written atomically). On the first start dengai only records the current
change number; packages are evaluated from then on.

## HTTP API

All endpoints need `Authorization: Bearer $DENGAI_API_TOKEN`.

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

`/status` returns for example:

```json
{"connected":true,"steam_id":76561198000000000,
 "farming":{"appid":440,"name":"Team Fortress 2","drops":3},"games_to_farm":4,
 "farm_paused":false,"playing_blocked":false,"claim_queue":2,"change_number":31234567,
 "recent_claims":[{"id":"sub/123","result":"ok","at":1767225600}]}
```

## Docker (Orange Pi Zero 3)

Multi-arch images (`linux/arm64`, `linux/amd64`) are published to `ghcr.io/sudtanj/dengai`: a
`scratch` image with the static musl binary, CA certificates, user 65532 and a `/data` volume.

```sh
docker volume create dengai
docker run -d --name dengai --restart unless-stopped --network host \
  --memory 64m \
  -e DENGAI_USERNAME=myaccount \
  -e DENGAI_PASSWORD='...' \
  -e DENGAI_SHARED_SECRET='base64secret=' \
  -e DENGAI_API_TOKEN=change-me \
  -v dengai:/data \
  ghcr.io/sudtanj/dengai:latest
docker logs -f dengai
```

With `--network host` the API stays on the board's `127.0.0.1:8737`. To use bridge networking
instead, set `-e DENGAI_API_BIND=0.0.0.0:8737 -p 127.0.0.1:8737:8737`. Without a shared secret, run
the first login with `docker run -it ...` to type the Steam Guard code, or approve it in the app.

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
