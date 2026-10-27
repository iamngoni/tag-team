use serde::{Deserialize, Serialize};

use crate::{Arch, Bytes, Node, NodeId, NodeState, Os, OwnerPresence, RepoId, SubtaskSpec};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum NoFitReason {
    NoNodes,
    NodeNotActive {
        node: NodeId,
        state: NodeState,
    },
    OwnerPresent {
        node: NodeId,
    },
    OutsideAllowedHours {
        node: NodeId,
    },
    OnBattery {
        node: NodeId,
    },
    /// The owner account already has the configured number of active subtasks.
    ///
    /// This is deliberately account-wide rather than node-specific: each
    /// active subtask can keep a runner and a prepared workspace alive, even
    /// while it is waiting on main or collecting its result.
    AccountAtCapacity,
    NodeAtCapacity {
        node: NodeId,
    },
    NotEnoughCores {
        node: NodeId,
        needed: u16,
        available: u16,
    },
    NotEnoughMemory {
        node: NodeId,
        needed: Bytes,
        available: Bytes,
    },
    NotEnoughDisk {
        node: NodeId,
        needed: Bytes,
        available: Bytes,
    },
    WrongOs {
        node: NodeId,
        needed: Os,
        available: Os,
    },
    WrongArch {
        node: NodeId,
        needed: Arch,
        available: Arch,
    },
    MissingToolchain {
        node: NodeId,
        name: String,
        needed: String,
        available: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "placement", rename_all = "snake_case")]
pub enum Placement {
    Placed(NodeId),
    Queued(Vec<NoFitReason>),
}

impl Placement {
    #[must_use]
    pub fn node(&self) -> Option<NodeId> {
        match self {
            Self::Placed(node) => Some(*node),
            Self::Queued(_) => None,
        }
    }
}

#[must_use]
pub fn place(spec: &SubtaskSpec, repo: &RepoId, nodes: &[Node], siblings: &[NodeId]) -> Placement {
    if nodes.is_empty() {
        return Placement::Queued(vec![NoFitReason::NoNodes]);
    }

    let mut rejected = Vec::new();
    let mut viable = Vec::new();
    for node in nodes {
        match rejection(spec, node) {
            Some(reason) => rejected.push(reason),
            None => viable.push(node),
        }
    }

    viable
        .into_iter()
        .max_by_key(|node| score(node, spec, repo, siblings))
        .map_or_else(
            || Placement::Queued(rejected),
            |node| Placement::Placed(node.id),
        )
}

fn rejection(spec: &SubtaskSpec, node: &Node) -> Option<NoFitReason> {
    if node.state != NodeState::Active {
        return Some(NoFitReason::NodeNotActive {
            node: node.id,
            state: node.state,
        });
    }
    if !node
        .policy
        .allows_now(node.presence, node.capacity.local_minute_of_day)
    {
        return Some(
            if node.policy.only_when_idle && matches!(node.presence, OwnerPresence::Present) {
                NoFitReason::OwnerPresent { node: node.id }
            } else {
                NoFitReason::OutsideAllowedHours { node: node.id }
            },
        );
    }
    if node.policy.never_on_battery && (!node.capacity.battery_known || node.capacity.on_battery) {
        return Some(NoFitReason::OnBattery { node: node.id });
    }
    if node.capacity.running >= node.policy.max_subtasks {
        return Some(NoFitReason::NodeAtCapacity { node: node.id });
    }
    if let Some(os) = spec.needs.os
        && node.platform.os != os
    {
        return Some(NoFitReason::WrongOs {
            node: node.id,
            needed: os,
            available: node.platform.os,
        });
    }
    if let Some(arch) = spec.needs.arch
        && node.platform.arch != arch
    {
        return Some(NoFitReason::WrongArch {
            node: node.id,
            needed: arch,
            available: node.platform.arch,
        });
    }
    let needed_cores = spec.needs.cores.max(1);
    if node.capacity.cores_free < f32::from(needed_cores) {
        return Some(NoFitReason::NotEnoughCores {
            node: node.id,
            needed: needed_cores,
            available: node.capacity.cores_free as u16,
        });
    }
    if node.capacity.memory_free < spec.needs.memory {
        return Some(NoFitReason::NotEnoughMemory {
            node: node.id,
            needed: spec.needs.memory,
            available: node.capacity.memory_free,
        });
    }
    if node.capacity.disk_free < spec.needs.disk {
        return Some(NoFitReason::NotEnoughDisk {
            node: node.id,
            needed: spec.needs.disk,
            available: node.capacity.disk_free,
        });
    }
    for (name, required) in &spec.needs.toolchains {
        let found = node
            .toolchains
            .iter()
            .find(|toolchain| &toolchain.name == name);
        if !found.is_some_and(|toolchain| required.matches(&toolchain.version)) {
            return Some(NoFitReason::MissingToolchain {
                node: node.id,
                name: name.clone(),
                needed: required.to_string(),
                available: found.map(|toolchain| toolchain.version.to_string()),
            });
        }
    }
    None
}

fn score(node: &Node, spec: &SubtaskSpec, repo: &RepoId, siblings: &[NodeId]) -> i64 {
    let mut score = i64::from(node.capacity.cores_free as u16) * 10;
    score =
        score.saturating_add(i64::try_from(node.capacity.memory_free.0 / Bytes::GIB).unwrap_or(0));
    if node.repo_heads.iter().any(|cached| &cached.repo == repo) {
        score = score.saturating_add(25);
    }
    if siblings.contains(&node.id) {
        score = score.saturating_sub(30);
    }
    score = score.saturating_sub(i64::from(node.capacity.running) * 15);
    if spec.node_hint.as_deref() == Some(node.name.as_str()) {
        score = score.saturating_add(100);
    }
    score
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use semver::VersionReq;

    use super::*;
    use crate::{
        CachedRepo, Capacity, GitOid, HourRange, Limits, Needs, NodePolicy, Platform, Thermal,
        Timestamp, Toolchain, Trust,
    };

    fn spec() -> SubtaskSpec {
        SubtaskSpec {
            title: "tests".into(),
            prompt: "run them".into(),
            done_when: vec!["cargo test passes".into()],
            repo: PathBuf::from("/repo"),
            base_ref: "main".into(),
            include_uncommitted: true,
            needs: Needs::default(),
            limits: Limits::default(),
            egress: vec![],
            node_hint: None,
        }
    }

    fn node(name: &str, cores: f32) -> Node {
        Node {
            id: NodeId::new(),
            name: name.into(),
            platform: Platform {
                os: Os::MacOs,
                arch: Arch::Arm64,
                os_version: "test".into(),
            },
            state: NodeState::Active,
            cert_sha256: [1; 32],
            capacity: Capacity {
                cores_total: 10,
                cores_free: cores,
                memory_free: Bytes::gib(16),
                disk_free: Bytes::gib(50),
                running: 0,
                on_battery: false,
                battery_known: true,
                local_minute_of_day: Some(10 * 60),
                thermal: Thermal::Nominal,
            },
            toolchains: vec![],
            repo_heads: vec![],
            policy: NodePolicy {
                max_subtasks: 2,
                ..NodePolicy::default()
            },
            presence: OwnerPresence::Away {
                idle_for: Duration::from_secs(600),
            },
            last_seen: Timestamp::now(),
            trust: Trust::Own,
        }
    }

    #[test]
    fn picks_the_node_with_more_free_capacity() {
        let small = node("small", 2.0);
        let large = node("large", 8.0);
        let placement = place(
            &spec(),
            &RepoId(GitOid::zero()),
            &[small, large.clone()],
            &[],
        );
        assert_eq!(placement, Placement::Placed(large.id));
    }

    #[test]
    fn explains_a_missing_toolchain() {
        let mut node = node("mini", 8.0);
        node.toolchains.push(Toolchain {
            name: "cargo".into(),
            version: semver::Version::new(1, 70, 0),
            path: PathBuf::from("/cargo"),
        });
        let mut spec = spec();
        spec.needs
            .toolchains
            .push(("cargo".into(), VersionReq::parse(">=1.80").unwrap()));
        let placement = place(&spec, &RepoId(GitOid::zero()), &[node.clone()], &[]);
        assert!(matches!(
            placement,
            Placement::Queued(ref reasons)
                if matches!(reasons.first(), Some(NoFitReason::MissingToolchain { node: n, .. }) if *n == node.id)
        ));
    }

    #[test]
    fn cached_repo_bonus_breaks_a_tie() {
        let repo = RepoId(GitOid::zero());
        let mut plain = node("plain", 4.0);
        let mut cached = node("cached", 4.0);
        cached.repo_heads.push(CachedRepo {
            repo: repo.clone(),
            heads: vec![],
        });
        plain.capacity.cores_free = 4.0;
        let placement = place(&spec(), &repo, &[plain, cached.clone()], &[]);
        assert_eq!(placement, Placement::Placed(cached.id));
    }

    #[test]
    fn no_match_reports_every_specific_reason() {
        let repo = RepoId(GitOid::zero());
        assert_eq!(
            place(&spec(), &repo, &[], &[]),
            Placement::Queued(vec![NoFitReason::NoNodes])
        );

        let mut paused = node("paused", 4.0);
        paused.state = NodeState::Paused;
        let mut present = node("present", 4.0);
        present.policy.only_when_idle = true;
        present.presence = OwnerPresence::Present;
        assert_eq!(
            place(&spec(), &repo, &[paused.clone(), present.clone()], &[],),
            Placement::Queued(vec![
                NoFitReason::NodeNotActive {
                    node: paused.id,
                    state: NodeState::Paused,
                },
                NoFitReason::OwnerPresent { node: present.id },
            ])
        );
    }

    #[test]
    fn node_local_clock_controls_owner_hours_and_unknown_evidence_fails_closed() {
        let repo = RepoId(GitOid::zero());
        let mut node = node("local-clock", 4.0);
        node.policy.hours = Some(vec![HourRange {
            start_minutes: 9 * 60,
            end_minutes: 17 * 60,
        }]);

        node.capacity.local_minute_of_day = Some(10 * 60);
        assert_eq!(
            place(&spec(), &repo, &[node.clone()], &[]),
            Placement::Placed(node.id)
        );

        node.capacity.local_minute_of_day = Some(22 * 60);
        assert_eq!(
            place(&spec(), &repo, &[node.clone()], &[]),
            Placement::Queued(vec![NoFitReason::OutsideAllowedHours { node: node.id }])
        );

        node.capacity.local_minute_of_day = None;
        assert_eq!(
            place(&spec(), &repo, &[node.clone()], &[]),
            Placement::Queued(vec![NoFitReason::OutsideAllowedHours { node: node.id }])
        );
    }

    #[test]
    fn unknown_battery_evidence_fails_closed_when_battery_is_forbidden() {
        let repo = RepoId(GitOid::zero());
        let mut node = node("battery-unknown", 4.0);
        node.capacity.battery_known = false;
        assert_eq!(
            place(&spec(), &repo, &[node.clone()], &[]),
            Placement::Queued(vec![NoFitReason::OnBattery { node: node.id }])
        );
        node.capacity.battery_known = true;
        node.capacity.on_battery = false;
        assert_eq!(
            place(&spec(), &repo, &[node.clone()], &[]),
            Placement::Placed(node.id)
        );
    }
}
