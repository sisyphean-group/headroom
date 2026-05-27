//! parse/format the PipeWire `default` metadata object. binding,
//! listening, and writing live in [`crate::pw::registry`].

use serde_json::Value;

pub const DEFAULT_AUDIO_SINK_KEY: &str = "default.audio.sink";

pub const TARGET_OBJECT_KEY: &str = "target.object";

pub const SPA_JSON_TYPE: &str = "Spa:String:JSON";

/// parse a `default.audio.sink` value (`{"name":"alsa_output.…"}`) into
/// a sink name. `None` on anything unrecognised — ignore rather than
/// crash the metadata listener.
#[must_use]
pub fn parse_default_sink_name(value: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(value.trim()).ok()?;
    parsed.get("name")?.as_str().map(str::to_owned)
}

/// format a `target.object` value pointing at `sink_name`.
#[must_use]
pub fn format_sink_target_value(sink_name: &str) -> String {
    // formatter is also called with observed (user-influenced) sink
    // names, so escape embedded quotes even though pipewire's never
    // contain them.
    let escaped = sink_name.replace('"', "\\\"");
    format!("{{\"name\":\"{escaped}\"}}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_sink_name_from_canonical_json() {
        let v = parse_default_sink_name("{\"name\":\"alsa_output.usb-foo\"}");
        assert_eq!(v.as_deref(), Some("alsa_output.usb-foo"));
    }

    #[test]
    fn parses_default_sink_name_with_whitespace() {
        let v = parse_default_sink_name("  {\"name\":\"x\"}\n");
        assert_eq!(v.as_deref(), Some("x"));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_default_sink_name("not json"), None);
        assert_eq!(parse_default_sink_name("{}"), None);
        assert_eq!(parse_default_sink_name("{\"name\":42}"), None);
    }

    #[test]
    fn formats_sink_target_round_trips() {
        let formatted = format_sink_target_value("alsa_output.usb-foo");
        let back = parse_default_sink_name(&formatted).unwrap();
        assert_eq!(back, "alsa_output.usb-foo");
    }

    #[test]
    fn formats_sink_target_escapes_embedded_quote() {
        let formatted = format_sink_target_value("we\"ird");
        // Should still be valid JSON.
        let back = parse_default_sink_name(&formatted).unwrap();
        assert_eq!(back, "we\"ird");
    }
}
