//! The Bridge Kit's topic grammar (Store kit spec §3): `{Root}/{Lane}/{instance}[/{target}[/{property}]]`.
//!
//! The root and the instance come from `--root` and `--instance`; each is one segment, as a target
//! and a property are — never containing `/`, `+` or `#`.

/// Whether `segment` can stand as one segment of a topic: non-empty, and no `/`, `+` or `#`.
pub fn valid_segment(segment: &str) -> bool {
    !segment.is_empty() && !segment.contains(['/', '+', '#']) && !segment.contains('\0')
}

/// One bridge's topics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topics {
    root: String,
    instance: String,
}

/// What an incoming topic is, among those the bridge subscribes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// `Desired/{instance}/{target}/{property}`: a requested value, or (empty) its retraction.
    DesiredProperty {
        /// The target.
        target: String,
        /// The property.
        property: String,
    },
    /// `Desired/{instance}/{target}`: several properties, atomically; never retained.
    DesiredTarget {
        /// The target.
        target: String,
    },
    /// `Command/{instance}`.
    Command,
    /// `Config/{instance}/{target}`.
    Config {
        /// The target.
        target: String,
    },
    /// `State/{instance}/{target}`: the bridge's own retained state, read back at start.
    State {
        /// The target.
        target: String,
    },
    /// `Status/{instance}`: a configured instance's own status, as its read-back subscription shows
    /// later writes to it (multi-device spec §4.5).
    Status,
    /// Anything else the subscriptions let through: a malformed topic.
    Unknown,
}

impl Topics {
    /// The topics of `instance` under `root`; an error names a segment that cannot stand.
    pub fn new(root: &str, instance: &str) -> Result<Topics, String> {
        if !valid_segment(root) {
            return Err(format!("the root `{root}` is not one topic segment (empty, or holding `/`, `+` or `#`)"));
        }
        if !valid_segment(instance) {
            return Err(format!("the instance `{instance}` is not one topic segment (empty, or holding `/`, `+` or `#`)"));
        }
        Ok(Topics { root: root.to_owned(), instance: instance.to_owned() })
    }

    /// The root, one topic segment.
    pub fn root(&self) -> &str {
        &self.root
    }

    /// The instance, one topic segment.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    fn lane(&self, lane: &str) -> String {
        format!("{}/{lane}/{}", self.root, self.instance)
    }

    /// `{Root}/Active/{instance}`: retained, last will `"false"`.
    pub fn active(&self) -> String {
        self.lane("Active")
    }

    /// `{Root}/Status/{instance}`: retained.
    pub fn status(&self) -> String {
        self.lane("Status")
    }

    /// `{Root}/Version/{instance}`: retained.
    pub fn version(&self) -> String {
        self.lane("Version")
    }

    /// `{Root}/State/{instance}/{target}`: retained.
    pub fn state(&self, target: &str) -> String {
        format!("{}/{target}", self.lane("State"))
    }

    /// `{Root}/Event/{instance}`: never retained.
    pub fn event(&self) -> String {
        self.lane("Event")
    }

    /// `{Root}/Error/{instance}`: never retained.
    pub fn error(&self) -> String {
        self.lane("Error")
    }

    /// `{Root}/Command/{instance}`.
    pub fn command(&self) -> String {
        self.lane("Command")
    }

    /// `{Root}/Desired/{instance}/#`: both write forms. The bridge subscribes to them separately
    /// ([`desired_target_filter`](Self::desired_target_filter),
    /// [`desired_property_filter`](Self::desired_property_filter)).
    pub fn desired_filter(&self) -> String {
        format!("{}/#", self.lane("Desired"))
    }

    /// `{Root}/Desired/{instance}/+`: target-level writes, never retained — subscribed
    /// retain-as-published, so a live retained one keeps its flag and is refused.
    pub fn desired_target_filter(&self) -> String {
        format!("{}/+", self.lane("Desired"))
    }

    /// `{Root}/Desired/{instance}/+/+`: property writes, retained by design — subscribed without
    /// retain-as-published, so only a replay arrives flagged.
    pub fn desired_property_filter(&self) -> String {
        format!("{}/+/+", self.lane("Desired"))
    }

    /// Whether `topic` is on one of the bridge's request lanes — `Desired`, `Command` or `Config` —
    /// which the Store owns and the bridge reads.
    pub fn is_request_lane(&self, topic: &str) -> bool {
        self.on_own_lane(topic, &["Desired", "Command", "Config"])
    }

    /// Whether no reply may go to `topic`: one of the bridge's own lanes — those it reads (a reply
    /// there is read back as a request) and those it publishes but `Event` (`State`, `Error`,
    /// `Status`, `Active`, `Version`: a reply there passes for the bridge's own report) — or any
    /// instance's request lane under the same root, where another bridge would take it for a
    /// request (re-gate R17). The `Event` lane stays open: replies are Event-shaped, and the kit
    /// spec has them arrive there.
    pub fn is_reserved_for_replies(&self, topic: &str) -> bool {
        // Any kit request lane, by its shape — `{Root}/{Desired|Command|Config}/{instance}…` —
        // under any root: another family's bridge would take a reply there for a request (re-gate
        // 3 S6: only this bridge's root was guarded).
        let mut segments = topic.split('/');
        let any_request_lane = matches!(
            (segments.next(), segments.next(), segments.next()),
            (Some(root), Some("Desired" | "Command" | "Config"), Some(instance)) if !root.is_empty() && !instance.is_empty()
        );
        any_request_lane || self.on_own_lane(topic, &["State", "Error", "Status", "Active", "Version"])
    }

    fn on_own_lane(&self, topic: &str, lanes: &[&str]) -> bool {
        lanes.iter().any(|lane| {
            let lane = self.lane(lane);
            topic == lane || topic.strip_prefix(lane.as_str()).is_some_and(|rest| rest.starts_with('/'))
        })
    }

    /// `{Root}/Config/{instance}/#`.
    pub fn config_filter(&self) -> String {
        format!("{}/#", self.lane("Config"))
    }

    /// `{Root}/State/{instance}/#`: the start-up read-back.
    pub fn state_filter(&self) -> String {
        format!("{}/#", self.lane("State"))
    }

    /// `{Root}/State/{instance}/+`: a configured instance's read-back of its targets' `State`
    /// (multi-device spec §4.3, S1), kept for the life of its ownership.
    pub fn state_target_filter(&self) -> String {
        format!("{}/+", self.lane("State"))
    }

    /// `{Root}/Config/{instance}/+`: a configured instance's target Configs (multi-device spec
    /// §4.3, S2). A fixed bridge subscribes [`config_filter`](Self::config_filter).
    pub fn config_target_filter(&self) -> String {
        format!("{}/+", self.lane("Config"))
    }

    /// `{Root}/Desired/{instance}/{target}/{property}`.
    pub fn desired(&self, target: &str, property: &str) -> String {
        format!("{}/{target}/{property}", self.lane("Desired"))
    }

    /// `{Root}/Desired/{instance}/{target}`.
    pub fn desired_target(&self, target: &str) -> String {
        format!("{}/{target}", self.lane("Desired"))
    }

    /// `{Root}/Config/{instance}/{target}`.
    pub fn config(&self, target: &str) -> String {
        format!("{}/{target}", self.lane("Config"))
    }

    /// What `topic` is, of the lanes the bridge subscribes to.
    pub fn parse(&self, topic: &str) -> Inbound {
        let Some(rest) = topic.strip_prefix(self.root.as_str()).and_then(|r| r.strip_prefix('/')) else {
            return Inbound::Unknown;
        };
        let segments: Vec<&str> = rest.split('/').collect();
        if segments.len() < 2 || segments[1] != self.instance {
            return Inbound::Unknown;
        }
        let ok = |s: &str| valid_segment(s);
        match (segments[0], &segments[2..]) {
            ("Desired", [target]) if ok(target) => Inbound::DesiredTarget { target: (*target).to_owned() },
            ("Desired", [target, property]) if ok(target) && ok(property) => {
                Inbound::DesiredProperty { target: (*target).to_owned(), property: (*property).to_owned() }
            }
            ("Command", []) => Inbound::Command,
            ("Config", [target]) if ok(target) => Inbound::Config { target: (*target).to_owned() },
            ("State", [target]) if ok(target) => Inbound::State { target: (*target).to_owned() },
            ("Status", []) => Inbound::Status,
            _ => Inbound::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lanes_follow_the_grammar() {
        let t = Topics::new("Aircondition", "ctrl").unwrap();
        assert_eq!(t.active(), "Aircondition/Active/ctrl");
        assert_eq!(t.status(), "Aircondition/Status/ctrl");
        assert_eq!(t.state("L1.001"), "Aircondition/State/ctrl/L1.001");
        assert_eq!(t.desired("L1.001", "power"), "Aircondition/Desired/ctrl/L1.001/power");
        assert_eq!(t.desired_filter(), "Aircondition/Desired/ctrl/#");
        assert_eq!(t.desired_target_filter(), "Aircondition/Desired/ctrl/+");
        assert_eq!(t.desired_property_filter(), "Aircondition/Desired/ctrl/+/+");
        assert!(t.is_request_lane("Aircondition/Command/ctrl") && t.is_request_lane("Aircondition/Config/ctrl/u1"));
        assert!(t.is_request_lane("Aircondition/Desired/ctrl/u1/power"));
        assert!(!t.is_request_lane("Aircondition/Event/ctrl") && !t.is_request_lane("Aircondition/Commandx/ctrl"));
        assert!(!t.is_request_lane("Aircondition/Command/ctrlx"));
        for reserved in
            ["Aircondition/State/ctrl/u1", "Aircondition/Status/ctrl", "Aircondition/Command/other", "Aircondition/Desired/x/u/p"]
        {
            assert!(t.is_reserved_for_replies(reserved), "{reserved}");
        }
        assert!(!t.is_reserved_for_replies("Aircondition/Replies/ctrl") && !t.is_reserved_for_replies("Store/replies/1"));
        assert!(!t.is_reserved_for_replies("Aircondition/Event/ctrl"), "the Event lane, where replies arrive, was reserved");
        // Any kit request lane, by its shape, under any root (re-gate 3 S6).
        for other in ["Lutron/Command/x", "IR/Desired/tv/u1/power", "Dmx/Config/u/1"] {
            assert!(t.is_reserved_for_replies(other), "another family's request lane `{other}` was not reserved");
        }
        assert_eq!(t.command(), "Aircondition/Command/ctrl");
        assert_eq!(t.config_filter(), "Aircondition/Config/ctrl/#");
        assert_eq!(t.event(), "Aircondition/Event/ctrl");
        assert_eq!(t.error(), "Aircondition/Error/ctrl");
    }

    #[test]
    fn incoming_topics_are_classified() {
        let t = Topics::new("IR", "living").unwrap();
        assert_eq!(t.parse("IR/Desired/living/tv/power"), Inbound::DesiredProperty { target: "tv".into(), property: "power".into() });
        assert_eq!(t.parse("IR/Desired/living/tv"), Inbound::DesiredTarget { target: "tv".into() });
        assert_eq!(t.parse("IR/Command/living"), Inbound::Command);
        assert_eq!(t.parse("IR/Config/living/tv"), Inbound::Config { target: "tv".into() });
        assert_eq!(t.parse("IR/State/living/tv"), Inbound::State { target: "tv".into() });
        for unknown in [
            "IR/Desired/living",
            "IR/Desired/living/a/b/c",
            "IR/Desired/other/tv",
            "IRX/Command/living",
            "IR/Command/living/x",
            "IR/Desired/living//p",
        ] {
            assert_eq!(t.parse(unknown), Inbound::Unknown, "{unknown}");
        }
    }

    /// A configured instance's read-back subscription (multi-device spec §4.3, S1): its targets'
    /// `State` one level deep and its `Status`, both told apart on arrival.
    #[test]
    fn a_configured_instances_read_back_lanes() {
        let t = Topics::new("IR", "salon").unwrap();
        assert_eq!(t.state_target_filter(), "IR/State/salon/+");
        assert_eq!(t.config_target_filter(), "IR/Config/salon/+");
        assert_eq!(t.parse("IR/Status/salon"), Inbound::Status);
        assert_eq!(t.parse("IR/Status/salon/x"), Inbound::Unknown);
        assert_eq!(t.parse("IR/Status/other"), Inbound::Unknown);
    }

    #[test]
    fn a_root_or_instance_that_is_not_one_segment_is_refused() {
        assert!(Topics::new("A/B", "x").is_err());
        assert!(Topics::new("A", "x+").is_err());
        assert!(Topics::new("", "x").is_err());
        assert!(Topics::new("A", "").is_err());
    }
}
