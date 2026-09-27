# Conduit for developers

Conduit is a small native helper that runs on the user's machine and lets a
**website you build** reach parts of that machine — files, hardware info, apps,
local AI, a CORS-free fetch and more — but only after the user has granted your
site permission. This document is the API reference for talking to it from a
web page (or any HTTP client).

- It listens on **loopback only**: `http://127.0.0.1:8765` by default (the port
  is configurable with `--port`).
- Every request is **origin-scoped** and **permission-gated**; a human approves
  each site once at pairing, and the riskier actions ask again per call.
- Everything runs locally. Conduit has no cloud, no account, and no telemetry.

> This is the protocol reference. For the end-user explanation of pairing,
> consent and privacy, see the in-app **Help** page. For a feature/security
> overview, see [`README.md`](../README.md).

---

## 1. Quick start

```js
const BASE = "http://127.0.0.1:8765";

// 1. Is Conduit running? (public, CORS-enabled)
const health = await fetch(`${BASE}/health`).then(r => r.json()).catch(() => null);
if (!health?.ok) throw new Error("Conduit is not running");

// 2. Pair once — the user sees a prompt and picks which permissions to allow.
const pair = await fetch(`${BASE}/pair`, {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ perms: ["fs", "hw"] }),
}).then(r => r.json());
if (!pair.ok) throw new Error("pairing refused");
const token = pair.result.token;          // store this; it is your credential

// 3. Call methods.
const call = (method, params = {}) =>
  fetch(`${BASE}/rpc`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
    body: JSON.stringify({ id: 1, method, params }),
  }).then(r => r.json());

const info = await call("hw.info");
console.log(info.result.os.name);
```

The browser sends the `Origin` header automatically; Conduit uses it to
identify your site. Persist the `token` (e.g. `localStorage`) and reuse it —
re-pairing prompts the user again.

---

## 2. Transport & security model

| Rule | Why |
|---|---|
| Bound to `127.0.0.1` only | nothing off the machine can reach it |
| `Host` header must be loopback | blocks DNS-rebinding (`evil.com` → 127.0.0.1) |
| `Origin` header required, `http(s)` only | rejects `null`/`file://` origins and sandboxed iframes |
| One grant per origin, created only by a human at the pairing prompt | no drive-by access |
| Credential is a **Bearer token in a header**, never a cookie; only its SHA-256 is stored | no CSRF, nothing useful on disk |
| Permissions are scoped; sensitive actions re-prompt every call | least privilege |

Because the token is a header (not a cookie) and the `Origin`/`Host` are
checked, another site cannot ride your grant.

### CORS

`/health` is public and returns `Access-Control-Allow-Origin: *`. `/pair` and
`/rpc` reflect your `Origin` and answer `OPTIONS` preflights, so ordinary
`fetch` from your page works. Requests without a web `Origin` are refused.

---

## 3. Endpoints

| Method | Path | Auth | Purpose |
|---|---|---|---|
| `GET`  | `/health` | none | liveness, version, and the list of methods/permissions |
| `POST` | `/pair` | Origin | create (or refresh) this origin's grant; returns a token |
| `POST` | `/rpc` | Origin + Bearer | call a method |
| `GET`  | `/ws` | Origin, then `auth` frame | WebSocket for live events (file watching) |

There is also a `conduit://` URL protocol (registered on the user's request)
that only ever shows the app window — it never executes URL contents.

### `GET /health`

```json
{
  "ok": true,
  "name": "conduit",
  "version": "1.2.0",
  "perms": ["fs","hw","launch", "...", "shell","net"],
  "methods": ["ping","hw.info","fs.write", "..."]
}
```

Use it to detect Conduit and to feature-detect (check `methods`/`perms` before
relying on something new).

### `POST /pair`

Request body: `{ "perms": ["fs", "hw", ...] }` (a subset of the `perms` list
from `/health`; defaults to `["fs","hw"]` if omitted). The user may untick
permissions — you never receive more than they approved.

Response:

```json
{ "ok": true, "result": {
    "token": "…",           // your Bearer credential
    "perms": ["fs","hw"],   // what was actually granted
    "expires": null,          // Unix seconds, or null = never
    "session": false          // true = in-memory only, gone on app restart
} }
```

A refused pairing returns `{ "ok": false, "error": { "code": "denied", … } }`
with HTTP 403.

---

## 4. RPC envelope

Every `/rpc` call takes:

```json
{ "id": 1, "method": "fs.write", "params": { "path": "save.json", "data": "…" } }
```

`id` is echoed back and is otherwise opaque. Responses are one of:

```json
{ "id": 1, "ok": true,  "result": { … } }
{ "id": 1, "ok": false, "error": { "code": "denied", "message": "…" } }
```

### Error codes

`bad_host`, `bad_origin`, `unauthorized`, `denied`, `rate_limited`,
`bad_params`, `bad_path`, `not_found`, `not_utf8`, `too_large`, `exists`,
`quota`, `crypto`, `io`, `no_method`, `bad_json`, `net`,
`too_many_connections`.

`denied` covers both "you lack the permission" and "the user said no".

---

## 5. Permissions

Request only what you need. Each maps to a set of methods.

| Permission | Grants |
|---|---|
| `fs` | a private per-site sandbox folder (read/write/list/quota) |
| `hw` | hardware info and live stats (CPU, memory, disks, GPU, battery) |
| `launch` | list allowed apps, read app metadata/icons, launch approved apps |
| `system` | media keys, volume, now-playing, open URLs, request admin/elevation |
| `process` | list processes and end approved ones |
| `power` | lock / sleep / sign out / restart / shut down (always asks) |
| `clipboard` | write text; **reading always asks** |
| `notify` | desktop notifications (tagged with your site name) |
| `hostfs` | read (not change) any file on the PC; writes still ask each time |
| `crypto` | encrypt/decrypt your site's own data |
| `ai` | send prompts to a local AI model the user runs (Ollama/OpenAI-compatible) |
| `folder` | read/write a specific folder the user picks |
| `shell` | run a small set of read-only PowerShell cmdlets |
| `net` | fetch web pages/APIs on your behalf (a CORS-free proxy) |

---

## 6. Methods

Params are JSON. "asks" means Conduit shows the user a consent prompt for that
call. Paths in `fs.*`/`folder.*` are **relative** to the sandbox/granted root;
`..`, absolute paths, UNC and device names are rejected.

### Basics

| Method | Perm | Params → result |
|---|---|---|
| `ping` | – | → `{ pong: true }` |
| `perms` | – | → `{ origin, perms }` |
| `revoke` | – | drops this origin's grant |

### Sandbox files (`fs`)

Per-site private folder. Defaults: file ≤ 32 MiB, quota 256 MiB, ≤ 10 000 files.

| Method | Params |
|---|---|
| `fs.write` | `path`, `data`, `encoding` (`utf8`\|`base64`), `append` |
| `fs.read` | `path`, `encoding` |
| `fs.list` | `path` (default root) |
| `fs.stat`, `fs.mkdir` | `path` |
| `fs.delete` | `path`, `recursive` |
| `fs.copy`, `fs.move` | `from`, `to`, `overwrite` |
| `fs.reveal` | `path` (opens the file manager; throttled) |
| `fs.quota` | → `{ used, limit, files, max_files, max_file_size }` |

### User-picked folders (`folder`)

The user chooses a real folder; you address it by the returned `id`.

| Method | Params |
|---|---|
| `folder.pick` | `name`, `readOnly` — **asks**; → `{ id, name, path, read_only }` |
| `folder.granted` | → list of folders you hold |
| `folder.forget` | `id` |
| `folder.list`/`stat`/`read`/`write`/`mkdir`/`delete`/`move` | `folder` (the id) + the same path params as `fs.*` |

### Any file on the PC (`hostfs`)

| Method | Params |
|---|---|
| `host.roots` | → drive/home roots |
| `host.list` | `path` |
| `host.stat` | `path` → `{ exists, dir, size, readonly }` |
| `host.read` | `path`, `encoding` |
| `host.write`/`host.delete`/`host.mkdir`/`host.move` | `path` (+ `to`, `data`, …) — **each asks** |

### Apps (`launch`)

| Method | Params |
|---|---|
| `app.list` | → `{ allowed: [path…], apps: [{ path, name, file_name, size }] }` |
| `app.info` | `path` (absolute), `icon` (default `true`) → `{ name, version, publisher, description, size, modified, icon }`. `icon` is `{ mime:"image/png", data:"<base64>" }` on Windows, else `null`. |
| `app.launch` | `path` (absolute exe), `args` (array), `cwd` (inside the sandbox) — **asks unless remembered**; refuses `.bat .cmd .ps1 .vbs .js .lnk .scr .msi` |

### System (`system`)

| Method | Params |
|---|---|
| `sys.media` | `key` (`play_pause`/`next`/`prev`/`stop`/`volume_up`/`volume_down`/`mute`), `times` |
| `sys.volume` | → `{ level, muted }` |
| `sys.volume.set` | `level` (0–100) and/or `muted` |
| `sys.media.info` | → now-playing `{ present, title, artist, album, status, app }` |
| `sys.media.control` | `action` (`play`/`pause`/`toggle`/`next`/`prev`/`stop`) |
| `sys.open_url` | `url` (http/s; throttled) |
| `sys.elevation` | – (no perm) → `{ elevated }` |
| `sys.elevate` | — **asks**; relaunches elevated |

### Hardware (`hw`)

| Method | Params |
|---|---|
| `hw.info` | → `{ os, cpu, memory, disks }` (no serials/hostname/usernames) |
| `sys.stats` | → live `{ cpu, cores, mem_used, mem_total, net_rx, net_tx, uptime, battery }` |
| `sys.battery` | → `{ present, percent, charging, on_ac }` |
| `sys.gpu` | → `{ adapters:[{name,vendor,vram,shared}], usage }` |

### Processes / power

| Method | Perm | Params |
|---|---|---|
| `sys.processes` | `process` | `sort` (`memory`/`cpu`/`name`), `limit` |
| `sys.kill` | `process` | `pid` — **asks**; refuses PIDs ≤ 4 and critical processes |
| `sys.power` | `power` | `action` (`lock`/`sleep`/`logoff`/`restart`/`shutdown`/`abort`) — **asks** (except `abort`), 10 s grace |

### Clipboard / notifications

| Method | Perm | Params |
|---|---|---|
| `clipboard.write` | `clipboard` | `text` (≤ 1 MiB) |
| `clipboard.read` | `clipboard` | — **always asks** |
| `notify` | `notify` | `title`, `body` (throttled; tagged with your origin) |

### Encryption (`crypto`)

Encrypts data so only this origin (optionally + a password) can read it back.

| Method | Params |
|---|---|
| `crypto.encrypt` | `data`, `encoding`, optional `password` → `{ data: token }` |
| `crypto.decrypt` | `data` (token), optional `password`, `encoding` → `{ data }` |

### Local AI (`ai`)

Talks to a local model the user runs (Ollama, or an OpenAI-compatible endpoint).

| Method | Params |
|---|---|
| `ai.status` | → `{ online, kind, endpoint, models }` |
| `ai.generate` | `model`, `prompt`, optional `system` → `{ text }` |

### PowerShell (`shell`) — read-only, allow-listed

Only curated, read-only cmdlets run; arguments must be free of shell
metacharacters (no pipelines, `;`/`&`/`|`, `Invoke-Expression`). Each cmdlet has
a base risk; an **argument-aware classifier** raises it for wildcard sweeps,
remote `-ComputerName` queries and off-box targets. At or above the consent
threshold, the call **asks**, showing the reasons.

| Method | Params |
|---|---|
| `shell.commands` | → `{ commands:[{command,risk,needs_consent}], threshold, available }` |
| `shell.assess` | `command`, `args` → `{ command, base_risk, risk, needs_consent, reasons, preview }` (dry run — never executes) |
| `shell.run` | `command`, `args` → `{ command, risk, reasons, exit_code, output, truncated }` |

### Network proxy (`net`) — get around CORS

A server-side fetch so your page can reach APIs that block cross-origin browser
requests. It only reaches **public** hosts (loopback, private, link-local incl.
the cloud metadata IP, unique-local and CGNAT ranges are refused on IPv4 and
IPv6), re-validates every redirect hop, and caps bodies (response 8 MiB,
request 4 MiB, ≤ 5 redirects, 30 s). Rate-limited per origin.

`net.fetch`:

| Param | Meaning |
|---|---|
| `url` | absolute `http(s)` URL |
| `method` | `GET`(default)/`POST`/`PUT`/`PATCH`/`DELETE`/`HEAD` |
| `headers` | object of string values (hop-by-hop and `proxy-*`/`sec-*` are dropped) |
| `body` | request body string (for POST/PUT/PATCH) |
| `bodyEncoding` | `utf8` (default) or `base64` |
| `responseType` | `auto` (default; text if valid UTF-8, else base64), `text`, or `base64` |

Result:

```json
{ "status": 200, "status_text": "OK", "ok": true,
  "headers": { "content-type": "application/json" },
  "encoding": "text", "body": "…", "truncated": false,
  "final_url": "https://api.example.com/x", "redirected": false }
```

```js
const r = await call("net.fetch", {
  url: "https://api.example.com/data",
  headers: { accept: "application/json" },
});
const data = JSON.parse(r.result.body);
```

---

## 7. WebSocket events (`GET /ws`)

Open `ws://127.0.0.1:8765/ws`, then send frames as JSON. The **first** frame
must authenticate:

```json
{ "method": "auth", "params": { "token": "<your token>" } }
```

Then subscribe to file changes:

```json
{ "method": "watch", "params": { "scope": "sandbox" } }   // or "folder" + folder id
```

Events pushed to you:

```json
{ "event": "fs.change", "watch": 1, "changes": [ { "path": "a.txt", "kind": "added" } ] }
{ "event": "watch.stopped", "watch": 1, "reason": "revoked" }
```

Stop with `{ "method": "unwatch", "params": { "watch": 1 } }`.

---

## 8. Rate limits & quotas (defaults)

- Global token bucket **30 req/s** (burst 60); at most **4 WebSocket sockets**
  per origin.
- `sys.open_url` and `fs.reveal`: one per 2 s. `notify`: one per 1.5 s.
  `shell.run`: one per 0.5 s. `net.fetch`: ~20/s per origin.
- Sandbox: file ≤ 32 MiB, quota 256 MiB, ≤ 10 000 files (server-configurable).

Handle `rate_limited` and `quota` by backing off / informing the user.

---

## 9. Versioning & feature detection

`/health` reports `version` and the live `methods`/`perms` arrays. Prefer
feature-detecting against those over assuming a method exists, so your page
degrades gracefully against older installs. Conduit updates in place on Windows
(Velopack) and Linux AppImage builds; the protocol is additive.

---

## 10. Notes for building a page

- Always start with `/health`, then `/pair`, then cache the token.
- Request the **minimum** permissions; the user sees exactly what you ask for.
- Expect `denied` at any time (a user can revoke a grant from the app's **Sites**
  page) and re-pair when it happens.
- There is also a TurboWarp extension (`extension/conduit.js`) that wraps these
  methods as blocks, and a raw `call [method] with params [json]` escape hatch —
  handy for prototyping against new methods.
