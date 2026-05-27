//! blocking [`Client`].

use std::collections::VecDeque;
use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use headroom_ipc::{
    default_socket_path, Codec, Event, HelloData, Op, ProtoError, Request, Response,
    ResponsePayload, Route, ServerFrame, Status, Topic,
};

/// errors from the blocking client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("ipc: {0}")]
    Ipc(#[from] headroom_ipc::Error),

    /// server's first frame was not the `hello` event.
    #[error("expected hello event from server, got {0}")]
    BadHello(String),

    /// response id we never issued.
    #[error("response with unknown id {0}")]
    UnknownResponseId(u64),

    /// protocol-level error for an op.
    #[error("server error: {0}")]
    Protocol(#[from] ProtoError),

    #[error("no default socket path (XDG_RUNTIME_DIR unset and /proc/self/status unreadable)")]
    NoDefaultPath,

    /// typed-helper response failed to deserialize into the expected shape.
    #[error("response shape mismatch: {0}")]
    DecodeResult(serde_json::Error),
}

/// blocking client for the headroom control protocol.
///
/// single-threaded by construction; for concurrent request/response and event
/// consumption, open two connections.
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    codec: Codec,
    next_id: u64,
    pending_events: VecDeque<Event>,
    hello: HelloData,
    socket_path: PathBuf,
}

impl Client {
    /// connect at the default socket path.
    pub fn connect() -> Result<Self, ClientError> {
        let path = default_socket_path().ok_or(ClientError::NoDefaultPath)?;
        Self::connect_at(&path)
    }

    /// connect at the given socket path.
    pub fn connect_at(path: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path).map_err(|e| ClientError::Ipc(e.into()))?;
        let reader_half = stream.try_clone().map_err(|e| ClientError::Ipc(e.into()))?;
        let writer_half = stream;

        let mut me = Self {
            reader: BufReader::new(reader_half),
            writer: BufWriter::new(writer_half),
            codec: Codec::new(),
            next_id: 1,
            pending_events: VecDeque::new(),
            // placeholder; populated by handshake below
            hello: HelloData {
                daemon: String::new(),
                version: String::new(),
                protocol: 0,
            },
            socket_path: path.to_path_buf(),
        };

        me.handshake()?;
        Ok(me)
    }

    fn handshake(&mut self) -> Result<(), ClientError> {
        let frame: ServerFrame = self.codec.read(&mut self.reader)?;
        match frame {
            ServerFrame::Event(ev)
                if ev.topic == Topic::Control && ev.event.as_str() == "hello" =>
            {
                let hello: HelloData =
                    serde_json::from_value(ev.data).map_err(ClientError::DecodeResult)?;
                self.hello = hello;
            }
            ServerFrame::Event(ev) => {
                return Err(ClientError::BadHello(format!(
                    "{} event on {}",
                    ev.event, ev.topic
                )))
            }
            ServerFrame::Response(r) => {
                return Err(ClientError::BadHello(format!("response id={}", r.id)))
            }
        }

        // best effort for older daemons
        self.send_raw(Op::Hello {
            protocol: headroom_ipc::PROTOCOL_VERSION,
        })?;
        Ok(())
    }

    /// `hello` payload received on connect.
    #[must_use]
    pub fn hello(&self) -> &HelloData {
        &self.hello
    }

    /// socket path this client is connected to.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        // wrap: u64::MAX requests on one connection won't happen, but be correct
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// send a request, block until the paired response arrives.
    ///
    /// stray events received meanwhile are queued for [`next_event`](Self::next_event).
    pub fn send(&mut self, op: Op) -> Result<serde_json::Value, ClientError> {
        let payload = self.send_raw(op)?;
        match payload {
            ResponsePayload::Ok { result } => Ok(result),
            ResponsePayload::Err { error } => Err(ClientError::Protocol(error)),
        }
    }

    /// like [`send`](Self::send) but returns the raw [`ResponsePayload`] —
    /// keeps a protocol-level error in-band instead of as [`ClientError::Protocol`].
    pub fn send_raw(&mut self, op: Op) -> Result<ResponsePayload, ClientError> {
        let id = self.alloc_id();
        let req = Request::new(id, op);
        self.codec.write(&mut self.writer, &req)?;

        loop {
            let frame: ServerFrame = self.codec.read(&mut self.reader)?;
            match frame {
                ServerFrame::Response(Response {
                    id: rid,
                    payload: _,
                }) if rid != id => {
                    return Err(ClientError::UnknownResponseId(rid));
                }
                ServerFrame::Response(Response { payload, .. }) => return Ok(payload),
                ServerFrame::Event(ev) => {
                    self.pending_events.push_back(ev);
                }
            }
        }
    }

    /// block until the next event arrives. drains the queue before reading the socket.
    ///
    /// an unsolicited response (no matching request) is rejected as
    /// [`ClientError::UnknownResponseId`].
    pub fn next_event(&mut self) -> Result<Event, ClientError> {
        if let Some(ev) = self.pending_events.pop_front() {
            return Ok(ev);
        }
        match self.codec.read::<_, ServerFrame>(&mut self.reader)? {
            ServerFrame::Event(ev) => Ok(ev),
            ServerFrame::Response(r) => Err(ClientError::UnknownResponseId(r.id)),
        }
    }

    /// pop a queued event without blocking. does **not** read from the socket.
    pub fn pending_event(&mut self) -> Option<Event> {
        self.pending_events.pop_front()
    }

    // typed convenience wrappers

    fn send_into<T: DeserializeOwned>(&mut self, op: Op) -> Result<T, ClientError> {
        let value = self.send(op)?;
        serde_json::from_value(value).map_err(ClientError::DecodeResult)
    }

    /// `status`
    pub fn status(&mut self) -> Result<Status, ClientError> {
        self.send_into(Op::Status)
    }

    /// `profile.list`
    pub fn profile_list(&mut self) -> Result<Vec<headroom_ipc::ProfileInfo>, ClientError> {
        #[derive(serde::Deserialize)]
        struct Body {
            profiles: Vec<headroom_ipc::ProfileInfo>,
        }
        let body: Body = self.send_into(Op::ProfileList)?;
        Ok(body.profiles)
    }

    /// `profile.use`
    pub fn profile_use(&mut self, name: &str) -> Result<String, ClientError> {
        #[derive(serde::Deserialize)]
        struct Body {
            name: String,
        }
        let body: Body = self.send_into(Op::ProfileUse {
            name: name.to_owned(),
        })?;
        Ok(body.name)
    }

    /// `profile.show`
    pub fn profile_show(
        &mut self,
        name: Option<&str>,
    ) -> Result<serde_json::Value, ClientError> {
        self.send(Op::ProfileShow {
            name: name.map(String::from),
        })
    }

    /// `profile.reload`
    pub fn profile_reload(&mut self) -> Result<Vec<String>, ClientError> {
        #[derive(serde::Deserialize)]
        struct Body {
            reloaded: Vec<String>,
        }
        let body: Body = self.send_into(Op::ProfileReload)?;
        Ok(body.reloaded)
    }

    /// `route.list`
    pub fn route_list(&mut self) -> Result<headroom_ipc::RouteList, ClientError> {
        self.send_into(Op::RouteList)
    }

    /// `route.set`
    pub fn route_set(&mut self, app: &str, to: Route) -> Result<(), ClientError> {
        let _: serde_json::Value = self.send(Op::RouteSet {
            app: app.to_owned(),
            to,
        })?;
        Ok(())
    }

    /// `route.unset`
    pub fn route_unset(&mut self, app: &str) -> Result<(), ClientError> {
        let _: serde_json::Value = self.send(Op::RouteUnset {
            app: app.to_owned(),
        })?;
        Ok(())
    }

    /// `route.stream`
    pub fn route_stream(&mut self, node_id: u32, to: Route) -> Result<(), ClientError> {
        let _: serde_json::Value = self.send(Op::RouteStream { node_id, to })?;
        Ok(())
    }

    /// `setting.get`
    pub fn setting_get(&mut self, key: &str) -> Result<serde_json::Value, ClientError> {
        #[derive(serde::Deserialize)]
        struct Body {
            #[allow(dead_code)]
            key: String,
            value: serde_json::Value,
        }
        let body: Body = self.send_into(Op::SettingGet {
            key: key.to_owned(),
        })?;
        Ok(body.value)
    }

    /// `setting.set`
    pub fn setting_set(
        &mut self,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), ClientError> {
        let _: serde_json::Value = self.send(Op::SettingSet {
            key: key.to_owned(),
            value,
        })?;
        Ok(())
    }

    /// `setting.clear` — returns whether an override was present.
    pub fn setting_clear(&mut self, key: &str) -> Result<bool, ClientError> {
        #[derive(serde::Deserialize)]
        struct Body {
            cleared: bool,
        }
        let body: Body = self.send_into(Op::SettingClear {
            key: key.to_owned(),
        })?;
        Ok(body.cleared)
    }

    /// `setting.reset` — returns how many overrides were cleared.
    pub fn setting_reset(&mut self) -> Result<usize, ClientError> {
        #[derive(serde::Deserialize)]
        struct Body {
            cleared: usize,
        }
        let body: Body = self.send_into(Op::SettingReset)?;
        Ok(body.cleared)
    }

    /// `bypass.set`
    pub fn bypass_set(&mut self, enabled: bool) -> Result<(), ClientError> {
        let _: serde_json::Value = self.send(Op::BypassSet { enabled })?;
        Ok(())
    }

    /// `per-app.list`
    pub fn layer_a_list(
        &mut self,
    ) -> Result<Vec<headroom_ipc::LayerASnapshot>, ClientError> {
        #[derive(serde::Deserialize)]
        struct Body {
            layer_a: Vec<headroom_ipc::LayerASnapshot>,
        }
        let body: Body = self.send_into(Op::LayerAList)?;
        Ok(body.layer_a)
    }

    /// `per-app.set`
    pub fn per_app_set(&mut self, app: &str, enabled: bool) -> Result<(), ClientError> {
        let _: serde_json::Value = self.send(Op::PerAppSet {
            app: app.to_owned(),
            enabled,
        })?;
        Ok(())
    }

    /// `per-app.master`
    pub fn per_app_master(&mut self, enabled: bool) -> Result<(), ClientError> {
        let _: serde_json::Value = self.send(Op::PerAppMaster { enabled })?;
        Ok(())
    }

    /// `per-app.reset`
    pub fn layer_a_reset(&mut self, node_id: u32) -> Result<(), ClientError> {
        let _: serde_json::Value = self.send(Op::LayerAReset { node_id })?;
        Ok(())
    }

    /// `subscribe`
    pub fn subscribe(&mut self, topics: &[Topic]) -> Result<Vec<Topic>, ClientError> {
        #[derive(serde::Deserialize)]
        struct Body {
            subscribed: Vec<Topic>,
        }
        let body: Body = self.send_into(Op::Subscribe {
            topics: topics.to_vec(),
        })?;
        Ok(body.subscribed)
    }

    /// `unsubscribe`
    pub fn unsubscribe(&mut self, topics: &[Topic]) -> Result<Vec<Topic>, ClientError> {
        #[derive(serde::Deserialize)]
        struct Body {
            unsubscribed: Vec<Topic>,
        }
        let body: Body = self.send_into(Op::Unsubscribe {
            topics: topics.to_vec(),
        })?;
        Ok(body.unsubscribed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, BufWriter};
    use std::os::unix::net::UnixStream;
    use std::thread;

    use headroom_ipc::{Codec, Event, HelloData, Op, Request, Response, ServerFrame, Topic};

    /// A tiny in-process server that runs on the other end of a
    /// `UnixStream::pair`. Knows just enough to exercise the client.
    fn spawn_test_server() -> (UnixStream, thread::JoinHandle<()>) {
        let (a, b) = UnixStream::pair().unwrap();
        let server_handle = thread::spawn(move || {
            let codec = Codec::new();
            let read_side = b.try_clone().unwrap();
            let mut reader = BufReader::new(read_side);
            let mut writer = BufWriter::new(b);

            // Send hello.
            let hello = Event::new(
                Topic::Control,
                "hello",
                &HelloData {
                    daemon: "headroom".into(),
                    version: "0.1.0-test".into(),
                    protocol: headroom_ipc::PROTOCOL_VERSION,
                },
            )
            .unwrap();
            codec
                .write(&mut writer, &ServerFrame::Event(hello))
                .unwrap();

            // Serve one round.
            loop {
                let req: Request = match codec.read(&mut reader) {
                    Ok(r) => r,
                    Err(_) => return,
                };
                let resp = match req.op {
                    Op::Status => Response::ok(
                        req.id,
                        &serde_json::json!({
                            "version": "0.1.0-test",
                            "protocol": headroom_ipc::PROTOCOL_VERSION,
                            "uptime_s": 0u64,
                            "profile": "default",
                            "bypass": false,
                            "sinks": {
                                "processed": {"ready": false},
                                "real":      {"ready": false},
                            },
                            "streams": []
                        }),
                    )
                    .unwrap(),
                    Op::ProfileUse { name } => {
                        Response::ok(req.id, &serde_json::json!({ "name": name })).unwrap()
                    }
                    Op::Subscribe { topics } => {
                        // Acknowledge, then push one event of each
                        // subscribed topic so the client can demonstrate
                        // event handling.
                        let body = serde_json::json!({ "subscribed": &topics });
                        let resp = Response::ok(req.id, &body).unwrap();
                        codec
                            .write(&mut writer, &ServerFrame::Response(resp.clone()))
                            .unwrap();

                        for t in topics {
                            let ev = Event::new(t, "tick", &serde_json::json!({})).unwrap();
                            codec.write(&mut writer, &ServerFrame::Event(ev)).unwrap();
                        }
                        continue;
                    }
                    _ => Response::ok(req.id, &serde_json::Value::Null).unwrap(),
                };
                codec
                    .write(&mut writer, &ServerFrame::Response(resp))
                    .unwrap();
            }
        });
        (a, server_handle)
    }

    fn client_on(stream: UnixStream) -> Client {
        let reader_half = stream.try_clone().unwrap();
        let writer_half = stream;
        let mut me = Client {
            reader: BufReader::new(reader_half),
            writer: BufWriter::new(writer_half),
            codec: Codec::new(),
            next_id: 1,
            pending_events: VecDeque::new(),
            hello: HelloData {
                daemon: String::new(),
                version: String::new(),
                protocol: 0,
            },
            socket_path: PathBuf::from("<test>"),
        };
        me.handshake().unwrap();
        me
    }

    #[test]
    fn handshake_then_status() {
        let (client_sock, _server) = spawn_test_server();
        let mut client = client_on(client_sock);
        assert_eq!(client.hello().daemon, "headroom");
        assert_eq!(client.hello().protocol, headroom_ipc::PROTOCOL_VERSION);

        let status = client.status().unwrap();
        assert_eq!(status.profile, "default");
        assert!(!status.bypass);
    }

    #[test]
    fn profile_use_returns_name() {
        let (client_sock, _server) = spawn_test_server();
        let mut client = client_on(client_sock);
        let name = client.profile_use("night").unwrap();
        assert_eq!(name, "night");
    }

    #[test]
    fn subscribe_then_consume_event() {
        let (client_sock, _server) = spawn_test_server();
        let mut client = client_on(client_sock);
        let acked = client.subscribe(&[Topic::Meters]).unwrap();
        assert_eq!(acked, vec![Topic::Meters]);

        let ev = client.next_event().unwrap();
        assert_eq!(ev.topic, Topic::Meters);
        assert_eq!(ev.event, "tick");
    }

    #[test]
    fn events_interleaved_during_request_are_queued() {
        // The test server pushes events *after* the subscribe response,
        // so let's check that requesting another op afterwards drains
        // them through the queue.
        let (client_sock, _server) = spawn_test_server();
        let mut client = client_on(client_sock);
        client.subscribe(&[Topic::Meters, Topic::Profile]).unwrap();

        // Now issue another request. The server hasn't sent the events
        // until we read more, but our client will keep reading.
        let status = client.status().unwrap();
        assert_eq!(status.profile, "default");

        // We may have buffered events from the prior subscribe and from
        // any in flight; drain them.
        let mut topics = Vec::new();
        while let Some(ev) = client.pending_event() {
            topics.push(ev.topic);
        }
        // The events arrived between the subscribe-ack and the status
        // response; both should be queued.
        assert!(topics.contains(&Topic::Meters));
        assert!(topics.contains(&Topic::Profile));
    }
}
