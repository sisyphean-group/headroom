# headroom-dsp

dsp kernels for headroom. pure rust, no dependencies.

- `Limiter` — feed-forward true-peak brickwall; configurable oversampling (1/2/4/8×),
  lookahead, hold, release.
- `Compressor` — log-domain feed-forward; peak/rms detector, soft knee, attack/release,
  optional auto-makeup.
- `AttackRelease` — exponential envelope follower (peak / inverse-gain modes).
- `DelayLine`, `SlidingMaxBuffer`, `PolyphaseUpsampler`, `PolyphaseDownsampler` — building blocks.

`process_*` methods are allocation-free; construction allocates, so don't construct on the
audio thread.

## License

MPL-2.0.
