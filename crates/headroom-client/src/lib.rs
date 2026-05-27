//! blocking client for the headroom control protocol. entry point: [`Client`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod client;

pub use client::{Client, ClientError};

pub use headroom_ipc::{
    default_socket_path, Codec, DaemonEvent, Error as IpcError, ErrorCode, Event, HelloData,
    LayerALevel, LayerASnapshot, MeterTick, Op, ProfileEvent, ProfileInfo, ProtoError, Request,
    Response, ResponsePayload, Route, RouteList, RouteRule, RouteRuleMatch, RoutingEvent,
    ServerFrame, SinkInfo, Sinks, Status, StreamRoute, Topic, PROTOCOL_VERSION,
};
