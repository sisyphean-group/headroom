//! per-app Layer A analysis tap. an input `pw_stream` siblings the
//! managed source; the rt callback computes per-block peak/mean_sq and
//! pushes a [`MeasurementSample`] into a per-tap rtrb for the daemon-side
//! controller. links are built explicitly by the registry watcher (see
//! the `connect` note below).

use pipewire::{
    core::Core,
    keys,
    properties::properties,
    spa::{
        param::{
            audio::{AudioFormat, AudioInfoRaw},
            ParamType,
        },
        pod::{serialize::PodSerializer, Object, Pod, Value},
        utils::{Direction, SpaTypes},
    },
    stream::{Stream, StreamFlags, StreamListener},
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rtrb::{Consumer, Producer, RingBuffer};

use crate::error::DaemonError;

/// v0 stereo only.
const TAP_CHANNELS: u32 = 2;

/// per-tap measurement ring, in [`MeasurementSample`]s. ~1.3 s at a
/// 21 ms quantum.
const TAP_RING_CAPACITY: usize = 64;

/// one block's analysis output. 8 bytes; `Copy`.
#[derive(Debug, Clone, Copy)]
pub struct MeasurementSample {
    /// `max(|x|)`.
    pub peak: f32,
    /// `Σ(x²)/N`.
    pub mean_sq: f32,
}

struct TapState {
    producer: Producer<MeasurementSample>,
    /// ring-full drops; harmless (controller time constants are seconds).
    drops: u64,
    /// rt format errors logged off-thread
    format_errors: Arc<AtomicU64>,
}

/// one per-app Layer A tap. the explicit per-channel links are owned
/// by the `ManagedStream` wrapping this tap (see `pw::registry`).
pub struct StreamTap {
    stream: Stream,
    _listener: StreamListener<TapState>,
    source_node_id: u32,
    format_errors: Arc<AtomicU64>,
}

impl StreamTap {
    /// spawn a tap on `source_node_id`.
    ///
    /// # Errors
    /// [`DaemonError::PipeWire`] on stream construction / connection
    /// failure.
    pub fn start(
        core: &Core,
        source_node_id: u32,
    ) -> Result<(Self, Consumer<MeasurementSample>), DaemonError> {
        let (producer, consumer) = RingBuffer::<MeasurementSample>::new(TAP_RING_CAPACITY);
        let format_errors = Arc::new(AtomicU64::new(0));

        let node_name = format!("headroom-tap.{source_node_id}");
        let stream_name = format!("headroom-tap-{source_node_id}");
        let props = properties! {
            *keys::MEDIA_TYPE => "Audio",
            *keys::MEDIA_CATEGORY => "Capture",
            *keys::MEDIA_ROLE => "DSP",
            *keys::NODE_NAME => node_name.as_str(),
            *keys::NODE_DESCRIPTION => "Headroom Layer A analysis tap",
            *keys::NODE_DONT_RECONNECT => "true",
            "node.dont-move" => "true",
        };
        let stream = Stream::new(core, &stream_name, props)
            .map_err(|e| DaemonError::pipewire(format!("tap stream new: {e}")))?;

        let listener = stream
            .add_local_listener_with_user_data(TapState {
                producer,
                drops: 0,
                format_errors: format_errors.clone(),
            })
            .process(tap_process)
            .state_changed(move |_stream_ref, _data, old, new| {
                tracing::debug!(
                    source = source_node_id,
                    ?old,
                    ?new,
                    "Layer A tap state change"
                );
            })
            .register()
            .map_err(|e| DaemonError::pipewire(format!("tap register: {e}")))?;

        let format_bytes = build_format_pod_bytes()?;
        let format_pod = Pod::from_bytes(&format_bytes)
            .ok_or_else(|| DaemonError::pipewire("Pod::from_bytes"))?;
        let mut params: [&Pod; 1] = [format_pod];
        stream
            .connect(
                Direction::Input,
                // no target: WP won't wire `Stream/Output → Stream/Input`,
                // so passing the source id makes no link (confirmed via
                // pw-cli). connect still creates our input ports from the
                // declared format — what the registry watcher's explicit
                // link-factory step needs.
                None,
                StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(|e| DaemonError::pipewire(format!("tap connect: {e}")))?;

        // without `AUTOCONNECT` the stream stays inactive after connect;
        // `set_active(true)` lifts Paused → Streaming once format locks
        // in (process only fires in Streaming).
        if let Err(e) = stream.set_active(true) {
            tracing::warn!(
                source = source_node_id,
                error = %e,
                "tap set_active failed; stream will stay Paused and no samples will flow"
            );
        }

        tracing::info!(
            source = source_node_id,
            "Layer A tap stream connected to source; awaiting Streaming state"
        );

        Ok((
            Self {
                stream,
                _listener: listener,
                source_node_id,
                format_errors,
            },
            consumer,
        ))
    }

    /// node id of the *source* stream this tap observes.
    #[must_use]
    pub fn source_node_id(&self) -> u32 {
        self.source_node_id
    }

    /// node id PipeWire assigned to *this* tap's stream; 0 until bound.
    #[must_use]
    pub fn tap_node_id(&self) -> u32 {
        self.stream.node_id()
    }

    /// total rt format errors
    #[must_use]
    pub fn format_error_count(&self) -> u64 {
        self.format_errors.load(Ordering::Relaxed)
    }
}

fn build_format_pod_bytes() -> Result<Vec<u8>, DaemonError> {
    // rate left unset (libspa omits SPA_FORMAT_AUDIO_rate when 0) so
    // PipeWire negotiates the source's rate. hardcoding 48 kHz would
    // wedge 44.1 kHz sources at Paused. block period varies with the
    // source quantum; the controller's alpha math handles it.
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_channels(TAP_CHANNELS);
    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|e| DaemonError::pipewire(format!("tap format pod: {e}")))?
        .0
        .into_inner();
    Ok(bytes)
}

/// rt `process` callback. allocation-free, guarded by
/// [`assert_no_alloc::assert_no_alloc`] in debug builds.
fn tap_process(stream: &pipewire::stream::StreamRef, state: &mut TapState) {
    assert_no_alloc::assert_no_alloc(|| tap_process_inner(stream, state));
}

fn tap_process_inner(stream: &pipewire::stream::StreamRef, state: &mut TapState) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        return;
    };
    let n_bytes = data.chunk().size() as usize;
    if n_bytes == 0 {
        return;
    }
    let Some(byte_slice) = data.data() else {
        return;
    };
    // chunk size is producer metadata; clamp before slicing
    let n = n_bytes.min(byte_slice.len());
    let samples: &[f32] = match bytemuck::try_cast_slice::<u8, f32>(&byte_slice[..n]) {
        Ok(s) => s,
        Err(_) => {
            // count on the rt path; log off-thread
            state.format_errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    if samples.is_empty() {
        return;
    }
    let mut peak = 0.0_f32;
    let mut sumsq = 0.0_f32;
    for &s in samples {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        sumsq += s * s;
    }
    let mean_sq = sumsq / samples.len() as f32;

    if state
        .producer
        .push(MeasurementSample { peak, mean_sq })
        .is_err()
    {
        // ring full — drop silently; a missed block is harmless.
        state.drops = state.drops.saturating_add(1);
    }
}
