//! per-subscriber event fan-out. full queue → event dropped + per-(sub,
//! topic) counter, flushed as a `daemon::overflow` event on next success.
//! see `IPC.md` §4 Backpressure.

use std::collections::{HashMap, HashSet};

use crossbeam_channel::{Sender, TrySendError};
use serde_json::json;

use headroom_ipc::{Event, ServerFrame, Topic};

pub const SUBSCRIBER_CAPACITY: usize = 64;

/// stable for the life of the connection.
pub type SubscriberId = u64;

struct Subscriber {
    tx: Sender<ServerFrame>,
    topics: HashSet<Topic>,
    /// per-topic drops since the last successful overflow flush.
    dropped: HashMap<Topic, u64>,
    /// lifetime total, reset only on subscriber removal.
    dropped_total: u64,
}

#[derive(Default)]
pub struct Broadcaster {
    subscribers: HashMap<SubscriberId, Subscriber>,
    next_id: SubscriberId,
}

impl std::fmt::Debug for Broadcaster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Broadcaster")
            .field("subscribers", &self.subscribers.len())
            .field("next_id", &self.next_id)
            .finish()
    }
}

impl Broadcaster {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// register a connection's outbound channel.
    pub fn register(&mut self, tx: Sender<ServerFrame>) -> SubscriberId {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.subscribers.insert(
            id,
            Subscriber {
                tx,
                topics: HashSet::new(),
                dropped: HashMap::new(),
                dropped_total: 0,
            },
        );
        id
    }

    /// idempotent.
    pub fn unregister(&mut self, id: SubscriberId) {
        self.subscribers.remove(&id);
    }

    /// add `topics` to subscriber `id`'s interest set; returns the
    /// accepted set.
    pub fn subscribe(&mut self, id: SubscriberId, topics: &[Topic]) -> Vec<Topic> {
        let Some(sub) = self.subscribers.get_mut(&id) else {
            return Vec::new();
        };
        for t in topics {
            // `control` is reserved for the connect-time hello.
            if matches!(t, Topic::Control) {
                continue;
            }
            sub.topics.insert(*t);
        }
        sub.topics.iter().copied().collect()
    }

    pub fn unsubscribe(&mut self, id: SubscriberId, topics: &[Topic]) -> Vec<Topic> {
        let Some(sub) = self.subscribers.get_mut(&id) else {
            return Vec::new();
        };
        for t in topics {
            sub.topics.remove(t);
        }
        topics.to_vec()
    }

    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// publish on `topic`: non-blocking try-send to each interested
    /// subscriber; full-queue failures accrue to the drop counter.
    pub fn publish(&mut self, topic: Topic, event: Event) {
        for sub in self.subscribers.values_mut() {
            if !sub.topics.contains(&topic) {
                continue;
            }
            match sub.tx.try_send(ServerFrame::Event(event.clone())) {
                Ok(()) => {
                    if sub.topics.contains(&Topic::Daemon) {
                        flush_overflow(sub);
                    }
                }
                Err(TrySendError::Full(_)) => {
                    *sub.dropped.entry(topic).or_insert(0) += 1;
                    sub.dropped_total = sub.dropped_total.wrapping_add(1);
                }
                Err(TrySendError::Disconnected(_)) => {
                    // reaped on unregister.
                }
            }
        }
    }
}

/// flush pending per-topic overflow counts as `daemon::overflow`
/// events; failed entries stay in `dropped` for next time.
fn flush_overflow(sub: &mut Subscriber) {
    if sub.dropped.is_empty() {
        return;
    }
    // drain first so a failed try_send doesn't double-emit.
    let entries: Vec<(Topic, u64)> = sub.dropped.drain().collect();
    for (lost_topic, lost) in entries {
        let lost_u32: u32 = u32::try_from(lost).unwrap_or(u32::MAX);
        let data = json!({
            "lost_topic": lost_topic.as_str(),
            "lost": lost_u32,
            "total_lost": sub.dropped_total,
        });
        let event = match Event::new(Topic::Daemon, "overflow", &data) {
            Ok(e) => e,
            Err(_) => continue,
        };
        match sub.tx.try_send(ServerFrame::Event(event)) {
            Ok(()) => {}
            Err(_) => {
                // reinstate the count for the next flush.
                *sub.dropped.entry(lost_topic).or_insert(0) += lost;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;
    use serde_json::Value;

    fn make_subscriber(b: &mut Broadcaster, capacity: usize) -> (SubscriberId, crossbeam_channel::Receiver<ServerFrame>) {
        let (tx, rx) = bounded(capacity);
        let id = b.register(tx);
        (id, rx)
    }

    fn ev(topic: Topic, name: &str) -> Event {
        Event::new(topic, name, &json!({})).unwrap()
    }

    #[test]
    fn register_and_unregister() {
        let mut b = Broadcaster::new();
        assert_eq!(b.subscriber_count(), 0);
        let (id, _rx) = make_subscriber(&mut b, 4);
        assert_eq!(b.subscriber_count(), 1);
        b.unregister(id);
        assert_eq!(b.subscriber_count(), 0);
    }

    #[test]
    fn publish_reaches_only_subscribed_topic_subscribers() {
        let mut b = Broadcaster::new();
        let (id_a, rx_a) = make_subscriber(&mut b, 4);
        let (id_b, rx_b) = make_subscriber(&mut b, 4);
        b.subscribe(id_a, &[Topic::Routing]);
        b.subscribe(id_b, &[Topic::Profile]);

        b.publish(Topic::Routing, ev(Topic::Routing, "stream_routed"));
        // A receives, B does not.
        assert!(rx_a.try_recv().is_ok());
        assert!(rx_b.try_recv().is_err());
    }

    #[test]
    fn control_topic_is_not_user_subscribable() {
        let mut b = Broadcaster::new();
        let (id, rx) = make_subscriber(&mut b, 4);
        let acked = b.subscribe(id, &[Topic::Control]);
        // Control was filtered out, so the ack list is empty for
        // this single-topic subscribe.
        assert!(acked.is_empty());
        // Publishing on Control doesn't reach the client.
        b.publish(Topic::Control, ev(Topic::Control, "hello"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn overflow_accrues_and_flushes_on_next_success() {
        let mut b = Broadcaster::new();
        // Capacity 2 so the flush has room for one routing + one
        // overflow event after we've drained. Subscriber needs the
        // Daemon topic on its interest list to receive overflow
        // notices.
        let (id, rx) = make_subscriber(&mut b, 2);
        b.subscribe(id, &[Topic::Routing, Topic::Daemon]);

        // First two publishes fill the queue.
        b.publish(Topic::Routing, ev(Topic::Routing, "stream_routed"));
        b.publish(Topic::Routing, ev(Topic::Routing, "stream_routed"));
        // Subsequent publishes overflow — counted, not delivered.
        b.publish(Topic::Routing, ev(Topic::Routing, "stream_routed"));
        b.publish(Topic::Routing, ev(Topic::Routing, "stream_routed"));

        // Drain both messages so the queue is empty again.
        let _ = rx.recv().unwrap();
        let _ = rx.recv().unwrap();

        // Now publish again; this should succeed AND piggyback the
        // overflow notice on the daemon topic.
        b.publish(Topic::Routing, ev(Topic::Routing, "stream_routed"));

        // Collect remaining messages.
        let mut got: Vec<ServerFrame> = Vec::new();
        while let Ok(m) = rx.try_recv() {
            got.push(m);
        }
        // Expect one routing event + one daemon::overflow.
        let topics: Vec<Topic> = got
            .iter()
            .map(|f| match f {
                ServerFrame::Event(e) => e.topic,
                ServerFrame::Response(_) => panic!("no response expected"),
            })
            .collect();
        assert!(topics.contains(&Topic::Routing));
        assert!(topics.contains(&Topic::Daemon));

        // The daemon event has the overflow payload.
        let overflow = got
            .into_iter()
            .find_map(|f| match f {
                ServerFrame::Event(e) if e.topic == Topic::Daemon => Some(e),
                _ => None,
            })
            .unwrap();
        assert_eq!(overflow.event, "overflow");
        let data = overflow.data;
        assert_eq!(data["lost_topic"], Value::String("routing".into()));
        assert!(data["lost"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn unsubscribe_stops_delivery() {
        let mut b = Broadcaster::new();
        let (id, rx) = make_subscriber(&mut b, 4);
        b.subscribe(id, &[Topic::Routing]);
        b.publish(Topic::Routing, ev(Topic::Routing, "stream_routed"));
        rx.try_recv().unwrap();

        b.unsubscribe(id, &[Topic::Routing]);
        b.publish(Topic::Routing, ev(Topic::Routing, "stream_routed"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn disconnected_subscriber_is_a_silent_no_op_until_unregistered() {
        let mut b = Broadcaster::new();
        let (id, rx) = make_subscriber(&mut b, 4);
        b.subscribe(id, &[Topic::Routing]);
        drop(rx); // simulate the reader thread dying

        // Publish doesn't panic and doesn't error.
        b.publish(Topic::Routing, ev(Topic::Routing, "stream_routed"));
        // Subscriber still listed until explicit unregister.
        assert_eq!(b.subscriber_count(), 1);
        b.unregister(id);
        assert_eq!(b.subscriber_count(), 0);
    }
}
