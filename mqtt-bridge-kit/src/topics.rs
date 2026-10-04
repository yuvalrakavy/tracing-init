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
    DesiredProperty { target: String, property: String },
    /// `Desired/{instance}/{target}`: several properties, atomically; never retained.
    DesiredTarget { target: String },
    /// `Command/{instance}`.
    Command,
    /// `Config/{instance}/{target}`.
    Config { target: String },
    /// `State/{instance}/{target}`: the bridge's own retained state, read back at start.
    State { target: String },
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

    pub fn root(&self) -> &str {
        &self.root
    }

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

    /// `{Root}/Desired/{instance}/#`: both write forms.
    pub fn desired_filter(&self) -> String {
        format!("{}/#", self.lane("Desired"))
    }

    /// `{Root}/Config/{instance}/#`.
    pub fn config_filter(&self) -> String {
        format!("{}/#", self.lane("Config"))
    }

    /// `{Root}/State/{instance}/#`: the start-up read-back.
    pub fn state_filter(&self) -> String {
        format!("{}/#", self.lane("State"))
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

    #[test]
    fn a_root_or_instance_that_is_not_one_segment_is_refused() {
        assert!(Topics::new("A/B", "x").is_err());
        assert!(Topics::new("A", "x+").is_err());
        assert!(Topics::new("", "x").is_err());
        assert!(Topics::new("A", "").is_err());
    }
}
