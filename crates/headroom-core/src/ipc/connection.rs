//! per-connection handler. each client spawns a writer thread (owns
//! the write half, drains a bounded channel the broadcaster also feeds)
//! and runs the reader on the current thread (dispatches requests,
//! sends responses through the channel). split so events don't
//! interleave with request/response writes.

use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};

use headroom_ipc::{
    Codec, Event, HelloData, Op, Request, Response, ServerFrame, Topic, PROTOCOL_VERSION,
};

use crate::ipc::broadcast::{SubscriberId, SUBSCRIBER_CAPACITY};
use crate::ipc::ops;
use crate::state::SharedState;

const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
const DAEMON_NAME: &str = "headroom";

/// how often the writer wakes to check the shutdown flag.
const WRITER_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub fn handle_connection(
    stream: UnixStream,
    state: SharedState,
    shutdown: Arc<AtomicBool>,
) {
    let codec = Codec::new();
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "ipc conn: failed to clone stream");
            return;
        }
    };
    let mut reader = BufReader::new(reader_stream);

    let (outbound_tx, outbound_rx) = bounded::<ServerFrame>(SUBSCRIBER_CAPACITY);

    // hello before anything else lands on the wire.
    if outbound_tx.try_send(ServerFrame::Event(hello())).is_err() {
        tracing::warn!("ipc conn: outbound queue full at hello (capacity misconfigured?)");
        return;
    }

    let sub_id = state.lock().broadcaster.register(outbound_tx.clone());

    let writer_stream = stream;
    let writer_shutdown = shutdown.clone();
    let writer_handle = thread::Builder::new()
        .name("headroom-ipc-conn-writer".into())
        .spawn(move || writer_loop(writer_stream, outbound_rx, writer_shutdown))
        .expect("spawn writer thread");

    serve(&codec, &mut reader, &outbound_tx, &state, sub_id, &shutdown);

    // drop outbound_tx so the writer sees disconnection and exits.
    state.lock().broadcaster.unregister(sub_id);
    drop(outbound_tx);
    let _ = writer_handle.join();
}

fn hello() -> Event {
    let data = HelloData {
        daemon: DAEMON_NAME.into(),
        version: DAEMON_VERSION.into(),
        protocol: PROTOCOL_VERSION,
    };
    // unwrap: HelloData always serialises.
    Event::new(Topic::Control, "hello", &data).expect("hello serialises")
}

fn writer_loop(stream: UnixStream, rx: Receiver<ServerFrame>, shutdown: Arc<AtomicBool>) {
    let codec = Codec::new();
    let mut writer = BufWriter::new(stream);
    while !shutdown.load(Ordering::Relaxed) {
        match rx.recv_timeout(WRITER_POLL_INTERVAL) {
            Ok(frame) => {
                if let Err(e) = codec.write(&mut writer, &frame) {
                    tracing::warn!(error = %e, "ipc writer: codec write failed; closing");
                    return;
                }
                // flush after each frame so events land promptly.
                if let Err(e) = writer.flush() {
                    tracing::warn!(error = %e, "ipc writer: flush failed; closing");
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn serve<R: std::io::Read>(
    codec: &Codec,
    reader: &mut R,
    outbound: &Sender<ServerFrame>,
    state: &SharedState,
    sub_id: SubscriberId,
    shutdown: &AtomicBool,
) {
    while !shutdown.load(Ordering::Relaxed) {
        let req: Request = match codec.read(&mut *reader) {
            Ok(r) => r,
            Err(headroom_ipc::Error::Closed) => return,
            Err(headroom_ipc::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return
            }
            Err(e) => {
                tracing::warn!(error = %e, "ipc reader: read failed; closing");
                return;
            }
        };

        // subscribe/unsubscribe mutate the broadcaster directly so
        // sub_id needn't thread through every op handler.
        let response: Response = match &req.op {
            Op::Subscribe { topics } => {
                let acked = state.lock().broadcaster.subscribe(sub_id, topics);
                ops::ok_value(req.id, &serde_json::json!({ "subscribed": acked }))
            }
            Op::Unsubscribe { topics } => {
                let acked = state.lock().broadcaster.unsubscribe(sub_id, topics);
                ops::ok_value(req.id, &serde_json::json!({ "unsubscribed": acked }))
            }
            _ => ops::dispatch(&req, state),
        };

        if outbound.send(ServerFrame::Response(response)).is_err() {
            return; // writer gone
        }
    }
}
