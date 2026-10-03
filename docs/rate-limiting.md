# Rate limiting

The server limits how often one client can call the sign-in routes and the functions that write
(#23). The limits are counted in memory on each machine. A refused request gets `429 Too Many
Requests` with a `Retry-After` header.

Code: `crates/iron-oxide-app/src/server/rate_limit.rs` (groups, limits, middlewares),
`rate_limit/limiter.rs` (the bounded token bucket) and `rate_limit/client_ip.rs` (client IPs). The
client side of the contract is `crates/iron-oxide-app/src/rate_limit.rs`.

## What is limited

Each request belongs to at most one **group**. Each group has its own buckets, so using up one
group's limit does not affect another's.

| Group | Routes | Per IP | Per signed-in user |
|---|---|---|---|
| `auth_begin` | `passkey/sign-up/begin`, `passkey/sign-in/begin`, `passkey/add/begin`, `google/begin` | 30 at once, then 1 every 2 s | 10 at once, then 1 a minute |
| `auth_finish` | `passkey/sign-up/finish`, `passkey/sign-in/finish`, `passkey/add/finish` | 30 at once, then 1 every 2 s | 10 at once, then 1 a minute |
| `google_callback` | `GET`/`HEAD /auth/google/callback` | 30 at once, then 1 every 2 s | none |
| `session` | `auth/me`, `auth/sign-out` | 300 at once, then 5 a second | none |
| `account` | `passkey/remove`, `google/unlink`, `rename` | 60 at once, then 1 a second | 10 at once, then 1 a minute |
| `sign_out_everywhere` | `sign-out-everywhere` | 60 at once, then 1 a second | 10 at once, then 1 a minute |
| `account_data` | `/api/account/export`, `/api/account/import`, `/api/account/delete` | 30 at once, then 1 every 2 s | 10 at once, then 1 every 2 minutes |
| `write` | every other `POST`, `PUT`, `PATCH` or `DELETE`, on any path | 600 at once, then 10 a second | 120 at once, then 2 a second |

`sign_out_everywhere` has its own bucket because it is how an owner ends a stolen session: a
thief holding that session could otherwise empty the `account` bucket with renames and keep the
owner from using it. Its first call ends the thief's session too.

Route paths are under `/api/auth/` unless shown in full. Requests with a safe method (`GET`, `HEAD`,
`OPTIONS`, `TRACE`) are never limited, except the Google callback. That keeps page loads, assets,
`GET` server functions and the health checks (`/healthz`, `/readyz`) out of it.

The per-user limits only apply to requests whose session holds a user. A signed-out request only
counts against its IP.

The three sign-in groups (`auth_begin`, `auth_finish`, `google_callback`) also limit each **IPv6
`/48`** as a whole: 120 at once, then 2 a second. That is four `/64`s' worth (see
[Client IP](#client-ip)). All the `/64`s of a `/48` share it, and a `/48` can hold unrelated
subscribers (a carrier or hosting pool), so together they get 120 sign-ins at once, then 2 a
second.

A request counts against both limits or neither:
- a `/64` over its own limit spends nothing of its `/48`, so flooding cannot lock its neighbours
  out;
- a `/48` over its aggregate adds no `/64` to the per-IP table.

### Where the checks run

The layers, from the outside in:

1. the CSRF check;
2. the per-IP limit;
3. the session layer;
4. the per-user limit;
5. the handler.

- **Before the session and the database.** The per-IP limit runs before any of them, so a refused
  request loads no session, writes no row and sets no cookie.
- **After the CSRF check.** Otherwise a cross-site page, opened by anyone behind a shared IP, could
  fire no-cors `POST`s at the begin functions. The CSRF check refuses them anyway, but they would
  use up the whole IP's sign-in limits and lock everyone behind it out.
- **The Google callback.** It is a cross-site `GET` by design, so the CSRF check lets it
  through, and such a page could still hit it (with an `<img>` or a `fetch`, say). Two defences:
  - When the browser sends `Sec-Fetch-Dest` and it is not `document`, the request is refused with
    `403` before it counts. Every browser that sends the header marks Google's redirect back
    (a top-level navigation, in the window or the popup) as `document`.
  - The callback has its own bucket, so an older browser that sends no `Sec-Fetch-Dest` can at
    worst slow down Google callbacks for its IP, never passkey sign-ins.

  This is tested by `cross_site_requests_cannot_use_up_a_shared_ips_limits` and
  `the_callback_only_counts_navigations`.

### Why these numbers

- **Shared IPs.** A gym's Wi-Fi or a mobile carrier's NAT puts a whole room behind one IP address.
  The per-IP sign-in limits let a class of about 30 people sign in at once: one sign-in takes one or
  two begins and one finish. After that, each IP gets one begin every 2 s, which is plenty for
  people and slow for a script.
- **The known exposure (#59's reviews).** Each cookie-less begin creates one `sessions` row and one
  `auth_ceremonies` row, and the cleanup runs only every 6 hours. Without limits, 1,000 requests a
  second meant about 21.6 M rows of each between two cleanups. Now each IP can create at most 30
  rows at once, then 1 every 2 s: about 10,800 per IP between two cleanups. For IPv6 that is per
`/64`, and each `/48` gets at most four times that however many `/64`s it uses. The per-IP check
runs
  before the session is loaded, so a refused request writes nothing and sets no cookie (tested by
  `a_limited_begin_creates_no_session_and_no_ceremony`).
- **Google polling.** While a Google sign-in is open, the UI calls `me` every 2 s. That is 0.5 a
  second per person, so the `session` group allows 5 a second per IP for a room of people.
- **Offline sync.** The client retry queue (#30) sends a whole workout's logged sets at once when it
  comes back online. So `write` allows 120 at once per user, and 600 at once per IP for a room of
  people doing that together.
- **Account changes** (adding a passkey, linking or unlinking Google) are rare: 10 in a row per user
  is more than a real session needs.
- **Account data** (#22): an export reads, and an import writes, everything a user owns (an import
  body can be 16 MiB), and deleting the account is done once. 10 in a row per user covers an export
  and a few import attempts; after that, one every 2 minutes.

### Per cookie?

There is no per-cookie limit. The cookie comes from the client: a script can drop it and get a new
session on each request, so a per-cookie limit is easy to get around. The per-IP limit covers both
cases: cookie-less begins, and concurrent begins on one cookie (which can each leave a ceremony
row).

## Client IP

`CLIENT_IP_SOURCE` says where the client's address comes from:

- **`peer`** (the default): the TCP peer address. It cannot be spoofed, and it is right when
  clients connect directly (local development, a plain VM). Headers such as `Fly-Client-IP` or
  `X-Forwarded-For` are ignored.
- **`fly`** (set in `fly.toml`): behind Fly.io, every connection comes from Fly's proxy, from a
  private `172.16.x.x` address. With `peer`, every user would share one set of limits. In this mode
  the server reads `Fly-Client-IP`, which [Fly's proxy always
  sets](https://fly.io/docs/networking/request-headers/) to the address it saw. Fly's page does not
  state explicitly that a client-sent value is replaced. The one-off post-deploy check in
  [deploy.md](operations/deploy.md) confirms it. It only trusts the header when all of these hold:
  - the TCP peer is a private address: loopback, `10/8`, `172.16/12`, `192.168/16`,
    `100.64/10`, link-local, or IPv6 unique-local (`fc00::/7`, Fly's private network) or link-local.
    A connection from the internet cannot set its own bucket;
  - the header appears exactly once and holds one valid IP address.

  Otherwise the peer address is used. `X-Forwarded-For` is never read.

**Residual risk in `fly` mode:** another machine on the organisation's private network (another
Fly app in the same org, or `fly proxy`) connects from a private address and could send any
`Fly-Client-IP`. Only the organisation's own apps and members can do that.

**If a CDN or another proxy is ever put in front of Fly,** `Fly-Client-IP` becomes that proxy's
address and all users behind it share one set of limits. The client IP would then have to come
from that proxy's own header, which needs a new mode.

**IPv6** clients are keyed by their `/64` prefix: one host usually holds a whole `/64` and could
otherwise rotate addresses to get fresh limits. A `/48` holds 65,536 `/64`s and is easy to get (a
tunnel broker, a hosting provider). The sign-in groups therefore also limit each `/48` as a whole
(see [What is limited](#what-is-limited)). IPv4-mapped IPv6 addresses are keyed as IPv4.

## Memory

Each limiter is a token bucket (GCRA) that keeps one timestamp per key (IP, `/48` or user) in a
hash map. A key whose bucket has refilled carries no state, so dropping it changes nothing.

The number of keys is capped by count, not by time: 50,000 per limiter. There are 13 limiters:

- 6 per IP;
- 3 per IPv6 `/48`;
- 4 per user.

An entry takes 32 bytes, and a full table takes about 2.1 MiB. So the worst case is about
**27 MiB** in all, on a 512 MB machine.

When a new key arrives and its table is full:

- **The refilled keys are dropped**, and only those. A key that has not refilled is never evicted.
  Evicting it would hand it a fresh burst, and a client cycling through more keys than the table
  holds (easy with the `/64`s of one IPv6 `/48`) would then have no limit at all.
- **If none has refilled, the new key gets the group's policy:**
  - `auth_begin`, `auth_finish` and `google_callback` **fail closed**, for their per-IP, per-`/48`
    and per-user limiters alike. While the table stays full, **every** new client (or, for the
    per-user limiters, every new user) gets a `429`, until the earliest stored key refills.
    These groups create `sessions` and `auth_ceremonies` rows, so that is safer than letting
    everyone through untracked.
  - This is a possible global "no new sign-ins" lockout, and it costs this much to hold:
    - A key does not have to be limited to hold its slot: one allowed request keeps it for one
      period, which is 2 s for the sign-in limits.
    - Holding a table full takes 50,000 live keys, i.e. about 25,000 allowed requests a second
      per group, per machine, from 50,000 distinct IPv4 addresses or IPv6 `/64`s.
    - One IPv6 `/48` can place up to 120 `/64` keys at once (its aggregate burst) and keep about
      4 alive. Holding the table therefore takes about 12,500 `/48`s sustained, or about 420 for
      a single 2-second burst.
    - That is botnet scale. At that rate the `auth_begin` rows hurt the database first.
  - `session`, `account`, `sign_out_everywhere` and `write` **fail open**. The request goes through without its key
    being tracked, and a warning is logged. These only do something for a signed-in user, whose
    sign-in was itself limited. The per-user limits still apply, and locking every new client out
    of them would hurt more than it protects.

The sweep scans the whole table, so it only runs when it can free something:

- While a table is full, the limiter remembers the earliest moment a stored key refills and does
  not sweep before then, so a new key in between costs one hash lookup.
- Each time a stored key refills, the next new key pays one scan. Someone holding a full table
  with staggered keys controls how often that happens, up to the rates above.
- The "table full" warning is logged at most once a minute per limiter.

This is tested by `cycling_through_more_keys_than_the_table_holds_gains_nothing` (limiter),
`cycling_through_more_clients_than_the_table_holds_gains_nothing`,
`one_ipv6_48_cannot_multiply_its_sign_in_limit` and `memory_stays_bounded_under_many_client_ips`.
All three count requests on a frozen clock.

A refused request does not move its bucket further out, so hammering a limit does not lengthen it.

## Several machines

The counts live in each machine's memory. Fly can send one client's requests to different
machines, so with `N` machines running a client can get up to `N` times each limit. A restart or
deploy also resets the counts. This is accepted for now: the app runs one machine most of the time
(`min_machines_running = 1`, the others are suspended when idle). A shared store, such as a
Postgres table or Redis, would be needed for exact limits across machines.

## The 429 contract

Every refused request gets:

- status `429 Too Many Requests`;
- `Retry-After: N`: whole seconds, rounded up, at least 1. It is the time until the next request
  would be allowed;
- `Cache-Control: no-store`.

For server functions (paths under `/api/`) the body is the same JSON a server function's own error
has:

```json
{"message": "Too many requests. Please try again in 5 seconds.", "code": 429,
 "data": {"ServerError": {"message": "Too many requests. Please try again in 5 seconds.",
                          "code": 429, "details": {"retry_after_secs": 5}}}}
```

The Dioxus client decodes it as `ServerFnError::ServerError { code: 429, message, details:
Some({"retry_after_secs": N}) }`, so the client can read the delay without the header. The
message can be shown as it is (the account page does). The Google callback page gets the same
message as plain text.

**For the client error classification (#68) and the retry queue (#30):**

- a 429 is **retryable, after the delay and never sooner**;
- read the delay with `ApiFailure::classify(&error).retry_after_secs()` (`docs/api.md`), or
  `crate::rate_limit::retry_after(&error)`. It returns `Some(delay)` for a 429:
  `retry_after_secs` from `details`, or 30 s if the 429 arrived without its body. It returns `None`
  for any other error;
- when you retry a batch, wait the delay once for the whole batch, not per request.

## Giving a new write endpoint stricter limits

Every new `POST` server function already gets the `write` limits, per IP and per user. For an
endpoint that needs stricter ones (an import, an upload, deleting the account), in
`server/rate_limit.rs`:

1. add a variant to `RouteGroup` (and to `RouteGroup::ALL`);
2. add its `GroupLimits` to `Limits` and set them in `Limits::default()`. Set `per_user` for a
   per-user limit, and keep a `per_ip` limit as well. Declare each quota as a `const` there, so an
   invalid one fails the build instead of panicking at startup;
3. map the endpoint's path to the group in `ROUTES`;
4. add a test that bursts past the new limit (see `rate_limit/integration_tests.rs`).

`every_route_in_the_table_exists` fails if a path in `ROUTES` is not a real route, so renaming an
endpoint cannot silently drop its limit.

## Tests

- `rate_limit::limiter::tests`: bursts, refill, `Retry-After` rounding, the key cap and which keys
  are dropped.
- `rate_limit::client_ip::tests`: the IP policy, private ranges, IPv6 `/64` keys.
- `rate_limit::integration_tests`:
  - a burst past the limit gets 429 with `Retry-After` until the bucket refills (paused clock);
  - limits are per IP and per group;
  - cross-site requests cannot use up a shared IP's limits;
  - a spoofed `Fly-Client-IP` is ignored in `peer` mode, and in `fly` mode from a public peer;
  - cycling through more clients (IPv4 addresses, or the `/64`s of one `/48`) than the table
    holds gains nothing;
  - the callback only counts navigations (`Sec-Fetch-Dest`);
  - a flooding `/64` does not lock out the rest of its `/48`;
  - memory stays bounded under thousands of IPv4 and IPv6 clients, and no limited client is
    evicted;
  - the health checks are never limited;
  - every path in `ROUTES` exists;
  - with Postgres: a refused begin creates no rows, per-user limits are independent behind one IP,
    the 429 body matches the contract, and the default limits let 20 sign-ups from one IP through.
- `tests/startup.rs::the_sign_in_limit_is_per_client_behind_the_proxy`: the real binary in `fly`
  mode. It checks that the per-IP limit sees the real connection.
