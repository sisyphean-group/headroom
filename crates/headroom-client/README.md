# headroom-client

blocking rust client for the headroom control protocol.

```rust
use headroom_client::{Client, Route};
use headroom_ipc::Topic;

let mut client = Client::connect()?;
println!("connected to headroom {}", client.hello().version);

client.profile_use("night")?;
client.route_set("firefox", Route::Processed)?;
client.subscribe(&[Topic::Meters])?;

loop {
    let event = client.next_event()?;
    println!("{}/{}: {}", event.topic, event.event, event.data);
}
# Ok::<(), headroom_client::ClientError>(())
```

thin layer over [`headroom-ipc`](../headroom-ipc): re-exports the wire types and
adds a `Client` over a `UnixStream` that correlates responses by `id` and queues
stray events received mid-request.

## license

MPL-2.0. safe to depend on from non-GPL clients.
