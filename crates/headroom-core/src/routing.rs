//! pure routing policy; pipewire-free so it unit-tests without the daemon

use headroom_ipc::{Route, RouteRuleMatch};

use crate::profile::Profile;

/// subset of a pw node's props the routing engine needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PwNodeInfo {
    pub node_id: u32,
    /// e.g. `"Stream/Output/Audio"`, `"Audio/Sink"`
    pub media_class: Option<String>,
    /// kernel-sourced, highest reliability
    pub application_process_binary: Option<String>,
    /// client-set
    pub application_name: Option<String>,
    /// flatpak-set, trustworthy when present
    pub portal_app_id: Option<String>,
    /// rarely set
    pub media_role: Option<String>,
    /// `node.dont-move`; honoured by skipping routing entirely
    pub dont_move: bool,
    /// `None` if absent. `>2ch` forced to bypass: bus filter is stereo-only, so processing surround
    /// would drop channels or downmix unrequested.
    pub audio_channels: Option<u32>,
}

impl PwNodeInfo {
    #[must_use]
    pub fn is_routable_playback(&self) -> bool {
        !self.dont_move && self.media_class.as_deref() == Some("Stream/Output/Audio")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingDecision {
    Route(Route),
    Skip,
}

/// `Skip` if not routable playback. `bypass_global` overrides every rule. else first-match route,
/// falling back to the profile's `default_route`.
#[must_use]
pub fn evaluate(info: &PwNodeInfo, profile: &Profile, bypass_global: bool) -> RoutingDecision {
    if !info.is_routable_playback() {
        return RoutingDecision::Skip;
    }
    // real graph operation (4k explicit links to the real sink), not just a metadata write —
    // see PwCommand::ReevaluateAll + `set_global_bypass`
    if bypass_global {
        return RoutingDecision::Route(Route::Bypass);
    }
    // stereo-only bus filter: processing >2ch would drop channels or downmix unrequested. straight
    // to the real sink preserves the layout; pw's source-side adapter handles any downmix.
    if matches!(info.audio_channels, Some(ch) if ch > 2) {
        return RoutingDecision::Route(Route::Bypass);
    }
    for rule in &profile.rules {
        if matches(info, &rule.match_) {
            return RoutingDecision::Route(rule.route);
        }
    }
    RoutingDecision::Route(profile.default_route.route)
}

/// true iff every present matcher field has a value equal to the node's. empty fields = don't care.
pub(crate) fn matches(info: &PwNodeInfo, m: &RouteRuleMatch) -> bool {
    let any_match = |needle: &Option<String>, hay: &[String]| -> bool {
        if hay.is_empty() {
            return true;
        }
        match needle {
            Some(s) => hay.iter().any(|h| h == s),
            None => false,
        }
    };

    any_match(&info.application_process_binary, &m.process_binary)
        && any_match(&info.application_name, &m.application_name)
        && any_match(&info.portal_app_id, &m.portal_app_id)
        && any_match(&info.media_role, &m.media_role)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playback(binary: &str) -> PwNodeInfo {
        PwNodeInfo {
            node_id: 1,
            media_class: Some("Stream/Output/Audio".into()),
            application_process_binary: Some(binary.into()),
            ..Default::default()
        }
    }

    #[test]
    fn non_playback_streams_are_skipped() {
        let mut info = playback("firefox");
        info.media_class = Some("Stream/Input/Audio".into());
        let profile = Profile::default_v0();
        assert_eq!(evaluate(&info, &profile, false), RoutingDecision::Skip);
    }

    #[test]
    fn dont_move_opts_out() {
        let mut info = playback("firefox");
        info.dont_move = true;
        let profile = Profile::default_v0();
        assert_eq!(evaluate(&info, &profile, false), RoutingDecision::Skip);
    }

    #[test]
    fn surround_streams_force_bypass_regardless_of_rule_match() {
        // The default profile routes `firefox` to processed. A 5.1
        // firefox stream (rare but valid — some browser content
        // declares surround) must still bypass: the bus filter is
        // stereo-only and the explicit-link path would otherwise
        // drop FC/LFE/SL/SR (surround contract).
        let mut info = playback("firefox");
        info.audio_channels = Some(6);
        let profile = Profile::default_v0();
        assert_eq!(
            evaluate(&info, &profile, false),
            RoutingDecision::Route(Route::Bypass)
        );
    }

    #[test]
    fn stereo_and_mono_streams_follow_normal_rules() {
        // Sanity: the surround forcer only kicks in for >2ch.
        let profile = Profile::default_v0();
        for ch in [None, Some(1), Some(2)] {
            let mut info = playback("firefox");
            info.audio_channels = ch;
            assert_eq!(
                evaluate(&info, &profile, false),
                RoutingDecision::Route(Route::Processed),
                "channels={ch:?}"
            );
        }
    }

    #[test]
    fn application_name_only_rule_matches_stream_with_no_process_binary() {
        // The shape `route set` emits when expanded into an
        // `application_name`-keyed override. Verifies that a
        // stream missing `application.process.binary` (typical
        // of pw-cat, many CLI tools, some Flatpak wrappers) is
        // still matched by the user's intent.
        use headroom_ipc::{RouteRule, RouteRuleMatch};
        let mut profile = Profile::default_v0();
        // Override at the top of the rule list.
        profile.rules.insert(
            0,
            RouteRule {
                match_: RouteRuleMatch {
                    application_name: vec!["pw-cat".into()],
                    ..Default::default()
                },
                route: Route::Bypass,
            },
        );
        // Stream advertises only application.name = "pw-cat".
        let info = PwNodeInfo {
            node_id: 9,
            media_class: Some("Stream/Output/Audio".into()),
            application_process_binary: None,
            application_name: Some("pw-cat".into()),
            ..Default::default()
        };
        assert_eq!(
            evaluate(&info, &profile, false),
            RoutingDecision::Route(Route::Bypass)
        );
    }

    #[test]
    fn matches_bypass_rule_for_known_music_player() {
        let info = playback("mpv");
        let profile = Profile::default_v0();
        assert_eq!(
            evaluate(&info, &profile, false),
            RoutingDecision::Route(Route::Bypass)
        );
    }

    #[test]
    fn matches_processed_rule_for_browser() {
        let info = playback("firefox");
        let profile = Profile::default_v0();
        assert_eq!(
            evaluate(&info, &profile, false),
            RoutingDecision::Route(Route::Processed)
        );
    }

    #[test]
    fn falls_back_to_default_route_when_no_rule_matches() {
        let info = playback("some-obscure-binary");
        let profile = Profile::default_v0();
        // default_v0 has `default_route = Processed`.
        assert_eq!(
            evaluate(&info, &profile, false),
            RoutingDecision::Route(Route::Processed)
        );
    }

    #[test]
    fn first_matching_rule_wins() {
        // Build a profile whose first rule says everything matches
        // → bypass, and second rule contradicts. First should win.
        let mut profile = Profile::default_v0();
        profile.rules.clear();
        profile.rules.push(headroom_ipc::RouteRule {
            match_: RouteRuleMatch {
                process_binary: vec!["firefox".into()],
                ..Default::default()
            },
            route: Route::Bypass,
        });
        profile.rules.push(headroom_ipc::RouteRule {
            match_: RouteRuleMatch {
                process_binary: vec!["firefox".into()],
                ..Default::default()
            },
            route: Route::Processed,
        });
        let info = playback("firefox");
        assert_eq!(
            evaluate(&info, &profile, false),
            RoutingDecision::Route(Route::Bypass)
        );
    }

    #[test]
    fn empty_matcher_acts_as_wildcard() {
        let mut profile = Profile::default_v0();
        profile.rules.clear();
        profile.rules.push(headroom_ipc::RouteRule {
            match_: RouteRuleMatch::default(), // all fields empty
            route: Route::Bypass,
        });
        let info = playback("firefox");
        assert_eq!(
            evaluate(&info, &profile, false),
            RoutingDecision::Route(Route::Bypass)
        );
    }

    #[test]
    fn multiple_match_fields_are_anded() {
        let mut profile = Profile::default_v0();
        profile.rules.clear();
        profile.rules.push(headroom_ipc::RouteRule {
            match_: RouteRuleMatch {
                process_binary: vec!["firefox".into()],
                media_role: vec!["Communication".into()],
                ..Default::default()
            },
            route: Route::Bypass,
        });

        // process_binary matches but media_role doesn't (None on info).
        let info = playback("firefox");
        assert_ne!(
            evaluate(&info, &profile, false),
            RoutingDecision::Route(Route::Bypass)
        );

        // Both match.
        let mut info2 = playback("firefox");
        info2.media_role = Some("Communication".into());
        assert_eq!(
            evaluate(&info2, &profile, false),
            RoutingDecision::Route(Route::Bypass)
        );
    }

    #[test]
    fn portal_app_id_can_match_when_present() {
        let mut profile = Profile::default_v0();
        profile.rules.clear();
        profile.rules.push(headroom_ipc::RouteRule {
            match_: RouteRuleMatch {
                portal_app_id: vec!["com.discordapp.Discord".into()],
                ..Default::default()
            },
            route: Route::Processed,
        });
        let mut info = playback("DiscordWrapper");
        info.portal_app_id = Some("com.discordapp.Discord".into());
        assert_eq!(
            evaluate(&info, &profile, false),
            RoutingDecision::Route(Route::Processed)
        );
    }
}
