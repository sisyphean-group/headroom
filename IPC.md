# headroom IPC — wire protocol

normative spec of headroom's control protocol. version `1`.

the contract between the daemon and any client (the `headroom` CLI, the
`headroom-client` Rust crate, and third parties — Qt/QuickShell panel, Eww
widget, shell script). the Rust representation lives in `headroom-ipc`; this
document is authoritative.

---

## 1. transport

- **Type:** Unix-domain socket, `SOCK_STREAM`.
- **Path:** `${XDG_RUNTIME_DIR}/headroom/control.sock`, falling back to
  `/run/user/${UID}/headroom/control.sock` if `XDG_RUNTIME_DIR` is unset.
- **Permissions:** the parent directory is created `0700`, the socket
  itself `0600`. Authn/authz is purely filesystem-based, matching the
  conventions of PipeWire and Wayland.
- **Encoding:** UTF-8 JSON, one message per frame.

## 2. framing

Each frame is a 4-byte big-endian unsigned length followed by exactly
that many bytes of JSON payload.

```
+--------+--------+--------+--------+----...----+
|  len high                       low | payload |
+--------+--------+--------+--------+----...----+
```

- Maximum frame size: **1 MiB** (1 048 576 bytes). Larger frames are a
  protocol violation; the server closes the connection.
- The payload MUST be a single JSON value (object). Pretty-printing is
  permitted but discouraged.
- No trailing newline or NUL terminator inside the frame.

## 3. message shapes

Every payload is a JSON object with one of three top-level shapes,
distinguished by which discriminating field is present.

### 3.1 request — client → server

```json
{
  "id": <u64>,
  "op": "<string>",
  "args": <object | omitted>
}
```

- `id`: client-chosen identifier, **must be unique across in-flight
  requests on a connection**. The server echoes this verbatim in the
  paired response. Clients may reuse an `id` once they have received
  the corresponding response.
- `op`: operation name. See §5.
- `args`: optional argument object. May be omitted if the operation
  takes no arguments.

### 3.2 response — server → client

Exactly one response is emitted per request, with the same `id`.

```json
{ "id": <u64>, "result": <value> }
```

or

```json
{ "id": <u64>, "error": { "code": "<string>", "message": "<string>" } }
```

- Mutually exclusive: either `result` or `error`, never both. (Both
  fields together is a server bug.)
- `result` may be any JSON value, including `null` for operations that
  succeed with no data.
- `error.code` is a stable machine-readable string from §6.
- `error.message` is human-readable English. Not stable; do not pattern
  match.

### 3.3 event — server → client

```json
{
  "event": "<string>",
  "topic": "<string>",
  "data": <object>
}
```

- Events have no `id`. A client distinguishes events from responses by
  presence of `event` / `topic` (events) vs. `id` (responses).
- `topic`: subscription topic the event belongs to (§4).
- `event`: name of the event within that topic.
- A client only receives events for topics it has explicitly
  subscribed to, with one exception: every new connection receives a
  `hello` event before any other traffic (see §7).

## 4. subscriptions

A client opts in to event streams by calling `subscribe` with a list of
topics, and opts out with `unsubscribe`. Subscription state is
per-connection and resets when the connection closes.

### topics

| Topic     | Cadence            | Purpose |
|-----------|-------------------|---------|
| `meters`  | publish_hz (≤ 60) | Live loudness, peak, gain-reduction telemetry. |
| `profile` | on change         | Profile use/reload events. |
| `routing` | on change         | Rule changes; per-stream routing decisions. |
| `daemon`  | on change         | Lifecycle, errors, overflow notifications. |

### backpressure

The server maintains a bounded queue per subscriber per topic
(default 64 messages; topic-overrideable in profile). When a queue is
full at publish time, the new event is **dropped**, the per-(subscriber,
topic) drop counter increments, and a `daemon` `overflow` event is
emitted to the affected subscriber describing the loss:

```json
{
  "event": "overflow",
  "topic": "daemon",
  "data": { "lost_topic": "meters", "lost": 42, "total_lost": 197 }
}
```

`overflow` events themselves cannot be dropped — if the `daemon` queue
is also full, the server closes the connection. A well-behaved client
either drains promptly or filters topics it doesn't care about.

The control thread never blocks on a slow client. Audio is never
affected by subscriber behaviour: meter publishing is rate-limited,
runs on a dedicated thread, and reads from a non-blocking source.

---

## 5. operations

All operations are listed below with their full argument and result
schemas. `args` is omitted from the request when its schema is empty.

### catalogue

| op                 | args                                          | result                       |
|--------------------|-----------------------------------------------|------------------------------|
| `status`           | —                                             | `Status`                     |
| `profile.list`     | —                                             | `{ profiles: ProfileInfo[] }`|
| `profile.use`      | `{ name: string }`                            | `{ name: string }`           |
| `profile.show`     | `{ name?: string }`                           | `Profile`                    |
| `profile.reload`   | —                                             | `{ reloaded: string[] }`     |
| `route.list`       | —                                             | `RouteList`                  |
| `route.set`        | `{ app: string, to: "processed"\|"bypass" }`  | `null`                       |
| `route.unset`      | `{ app: string }`                             | `null`                       |
| `route.stream`     | `{ node_id: u32, to: "processed"\|"bypass" }` | `null`                       |
| `setting.get`      | `{ key: string }`                             | `{ key: string, value: any }`|
| `setting.set`      | `{ key: string, value: any }`                 | `null`                       |
| `setting.list`     | —                                             | `{ settings: object }`       |
| `setting.clear`    | `{ key: string }`                             | `{ key: string, cleared: bool }`|
| `setting.reset`    | —                                             | `{ cleared: u32 }`           |
| `bypass.set`       | `{ enabled: bool }`                           | `null`                       |
| `per-app.list`     | —                                             | `{ layer_a: LayerASnapshot[] }`|
| `per-app.set`      | `{ app: string, enabled: bool }`              | `null`                       |
| `per-app.master`   | `{ enabled: bool }`                           | `null`                       |
| `per-app.reset`    | `{ node_id: u32 }`                            | `null`                       |
| `subscribe`        | `{ topics: string[] }`                        | `{ subscribed: string[] }`   |
| `unsubscribe`      | `{ topics: string[] }`                        | `{ unsubscribed: string[] }` |
| `hello`            | `{ protocol: u32 }`                           | `HelloData`                  |

`hello` is an optional client→server handshake announcing the client's
protocol version; the daemon logs a warning on mismatch but still serves
the connection (advisory), and replies with its own `HelloData`. Clients
that never send it are not validated. The reference client sends it
automatically right after receiving the server's `hello` event.

`per-app.set` / `per-app.master` persist to the user overlay (an
enable/disable override layered on the active profile's `[per_app]`).
`per-app.reset` is a one-shot that clears a managed stream's deference
lock (user-ceiling / strict mode) so the controller resumes normal
level control. Both `status` and `per-app.list` carry the
`LayerASnapshot[]` for currently-managed streams.

### object schemas

#### `Status`

```json
{
  "version": "0.1.0",
  "protocol": 1,
  "uptime_s": 482,
  "profile": "default",
  "bypass": false,
  "per_app": true,
  "sinks": {
    "processed": { "node_id": 51, "ready": true },
    "real":      { "node_id": 35, "name": "alsa_output.pci-0000_00_1f.3.analog-stereo" }
  },
  "streams": [
    { "node_id": 73, "app": "firefox", "route": "processed" },
    { "node_id": 81, "app": "spotify", "route": "bypass" }
  ],
  "layer_a": [
    { "node_id": 73, "app": "firefox", "managed": true, "volume_lin": 0.71,
      "reduction_db": 2.9, "user_ceiling_lin": null, "deferred": false }
  ],
  "setting_overrides": { "agc.enabled": false }
}
```

`per_app` is the per-app master switch. `layer_a` lists per-app
controller state for managed streams (omitted when empty); see
`LayerASnapshot` below. `setting_overrides` (omitted when empty) lists
the active overlay setting overrides (dotted key → value) that shadow
the active profile — a `setting.set` persists here and survives
`profile.use`, so clients surface it (and `setting.clear` / `setting.reset`
remove entries).

#### `LayerASnapshot`

```json
{
  "node_id": 73,
  "app": "firefox",
  "managed": true,
  "volume_lin": 0.71,
  "reduction_db": 2.9,
  "user_ceiling_lin": 0.6,
  "deferred": false
}
```

`reduction_db` is the smoothed gain reduction the controller currently
asserts (`>= 0`; `0` = no cut). `volume_lin` is the last
`channelVolumes` value written (1.0 = unity). `user_ceiling_lin` is
present only while ceiling-mode deference is active; `deferred` is true
when strict-mode deference has locked the controller pending a
`per-app.reset`.

#### `ProfileInfo`

```json
{ "name": "default", "active": true, "description": "Gentle …" }
```

#### `Profile`

The full profile document. Identical to the TOML profile, serialized as
JSON.

#### `RouteList`

```json
{
  "rules": [
    { "match": { "process_binary": ["firefox"] }, "route": "processed" }
  ],
  "current": [
    { "node_id": 73, "app": "firefox", "route": "processed" }
  ],
  "default_route": "processed"
}
```

### setting keys

`setting.get`/`setting.set` use dotted keys into the active profile.
Examples:

- `agc.target_lufs` (float)
- `agc.enabled` (bool)
- `compressor.threshold_db` (float)
- `compressor.ratio` (float)
- `limiter.ceiling_dbtp` (float)
- `limiter.lookahead_ms` (float)
- `limiter.oversample` (integer, one of 1/2/4/8)
- `meters.publish_hz` (float)

Headroom rejects sets that would violate invariants (e.g.
`limiter.ceiling_dbtp > 0.0`). See §6 for error codes.

---

## 6. errors

`error.code` is one of:

| code              | meaning                                                      |
|-------------------|--------------------------------------------------------------|
| `INVALID_FRAME`   | Malformed framing or non-JSON payload. Connection is closed. |
| `INVALID_MESSAGE` | Valid JSON, but doesn't fit a known message shape.           |
| `UNKNOWN_OP`      | `op` does not name a known operation.                        |
| `INVALID_ARGS`    | `args` missing a required field, wrong type, or out of range.|
| `NOT_FOUND`       | Profile / app / stream / setting key does not exist.         |
| `CONFLICT`        | Operation would violate an invariant (e.g. ceiling > 0).     |
| `BUSY`            | Daemon transiently cannot serve the request (rare).          |
| `INTERNAL`        | Bug. Includes a `message` for debugging.                     |

Frame-level violations (`INVALID_FRAME` of size, framing, encoding)
cause the connection to be closed after the error is sent.
Message-level errors leave the connection open.

---

## 7. connection lifecycle

1. Client connects. Server immediately emits a `hello` event:

   ```json
   {
     "event": "hello",
     "topic": "control",
     "data": {
       "daemon": "headroom",
       "version": "0.1.0",
       "protocol": 1
     }
   }
   ```

   This event is **not** gated on subscription — every client gets it.

2. Client MAY send a `hello` op announcing its own protocol version (the
   reference client does this automatically). The daemon logs a warning
   on mismatch but still serves the connection, and replies with its
   `HelloData`. Clients that skip it are not validated.

3. Client sends requests; server replies. Client may `subscribe` to
   topics at any time and will start receiving events for those
   topics.

4. Either side may close the socket at any time. The server cleans up
   subscription state. Outstanding requests are dropped (no response).

There is no formal `bye`. Closing the socket is the protocol.

---

## 8. versioning

The protocol uses a single integer version number, currently `1`.

- **Additions** (new ops, new optional fields, new events, new error
  codes) do not bump the protocol version. Clients MUST ignore unknown
  fields on objects they receive and MUST be tolerant of new event
  topics they did not subscribe to (they should never see those, but
  belt and braces).
- **Removals or semantic changes** bump the protocol version. A client
  may declare its version with the `hello` op; the daemon logs a warning
  on mismatch but currently still serves the connection (advisory — it
  does not reject).

Clients SHOULD log a warning if the `protocol` value in the server's
`hello` event does not match the version they were built against, and
proceed.

---

## 9. example exchange

```
C → S  len=58
       {"id":1,"op":"profile.use","args":{"name":"night"}}

S → C  len=24
       {"id":1,"result":{"name":"night"}}

C → S  len=49
       {"id":2,"op":"subscribe","args":{"topics":["meters"]}}

S → C  len=37
       {"id":2,"result":{"subscribed":["meters"]}}

S → C  len=137
       {"event":"tick","topic":"meters","data":{
         "momentary_lufs":-19.3,"shortterm_lufs":-20.1,
         "integrated_lufs":-19.8,"true_peak_dbtp":-1.4,
         "gain_reduction_db":-2.1,"agc_gain_db":0.5
       }}
```

---

## 10. reference

The authoritative Rust binding to this protocol is the `headroom-ipc`
crate; the `headroom-client` crate wraps it with a blocking `Client`
(and an optional async `AsyncClient` behind the `async` feature). Both
live in this repository.

Third-party clients should target this document, not the Rust types,
to remain interoperable across implementations.
