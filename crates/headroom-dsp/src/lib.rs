//! dsp kernels. every `process_*` is allocation-free + bounded-time; `new` allocates (not rt-safe).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod agc;
mod compressor;
mod delay;
mod envelope;
mod level_envelopes;
mod limiter;
mod oversample;
mod sliding_max;
pub mod util;

pub use agc::{AgcGain, AgcGainConfig};
pub use compressor::{Compressor, CompressorConfig, Detector};
pub use delay::DelayLine;
pub use envelope::AttackRelease;
pub use level_envelopes::{LevelDecision, LevelEnvelopes, LevelEnvelopesConfig};
pub use limiter::{Limiter, LimiterConfig, SetConfigOutcome, SoftTierConfig};
pub use oversample::{design_lowpass_blackman, PolyphaseDownsampler, PolyphaseUpsampler};
pub use sliding_max::SlidingMaxBuffer;
