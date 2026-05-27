# poking the IPC by hand

length-prefixed JSON over a Unix socket. drive it from a shell with `socat`
and a tiny helper.

## send a single request

```sh
# Send `{"id":1,"op":"status"}` as one framed message.
python3 - "$XDG_RUNTIME_DIR/headroom/control.sock" <<'PY'
import json, socket, struct, sys, os
sock_path = sys.argv[1]
msg = json.dumps({"id": 1, "op": "status"}).encode()
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock_path)

def read_frame(s):
    buf = b""
    while len(buf) < 4: buf += s.recv(4 - len(buf))
    n = struct.unpack(">I", buf)[0]
    body = b""
    while len(body) < n: body += s.recv(n - len(body))
    return body

# Drop the hello.
hello = read_frame(s)
print("hello:", hello.decode())

s.sendall(struct.pack(">I", len(msg)) + msg)
print("reply:", read_frame(s).decode())
PY
```

## subscribe and tail meters

```sh
python3 - "$XDG_RUNTIME_DIR/headroom/control.sock" <<'PY'
import json, socket, struct, sys
sock_path = sys.argv[1]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.connect(sock_path)

def read_frame(s):
    buf = b""
    while len(buf) < 4: buf += s.recv(4 - len(buf))
    n = struct.unpack(">I", buf)[0]
    body = b""
    while len(body) < n: body += s.recv(n - len(body))
    return body

def send(msg):
    b = json.dumps(msg).encode()
    s.sendall(struct.pack(">I", len(b)) + b)

read_frame(s)  # hello
send({"id": 1, "op": "subscribe", "args": {"topics": ["meters"]}})
ack = json.loads(read_frame(s))
print("subscribed:", ack)
while True:
    ev = json.loads(read_frame(s))
    if ev.get("topic") == "meters":
        print(ev["data"])
PY
```

## notes

- frames are 4-byte big-endian length + UTF-8 JSON. no newlines, no
  NUL terminators.
- the server always emits one `hello` event on the `control` topic
  immediately after `accept()` — read it first.
- errors come back as `{"id": N, "error": {"code": "...", "message": "..."}}`.
  see `IPC.md` §6 for the error-code table.
- `socat` works too, but framing makes raw `socat` awkward — pipe via
  a tiny script that reads/writes length prefixes.
