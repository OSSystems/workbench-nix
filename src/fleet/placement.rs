//! Where a new workspace goes: the node an admin picked, or else the open
//! node with the most free memory now, `MemAvailable` less its reserve.
//! Either must have the workspace's boot memory free, its disk's size free
//! on disk, and as much memory as the workspace may grow to: the host's
//! memory is its node's. The node is picked once, at creation.

use workbench_model::fleet::NodeView;

/// What people are told when no node has room.
pub const NO_ROOM: &str = "No node has room for it now. Ask an admin.";

/// What a new workspace needs of its node, in bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct Needs {
    /// Free now: what it boots with.
    pub boot: i64,
    /// Free on disk: its disk's size.
    pub disk: u64,
    /// The node's memory: the most it may grow to; none for `auto`.
    pub ceiling: Option<i64>,
}

/// Whether a node has room for a workspace.
fn has_room(n: &NodeView, needs: Needs) -> bool {
    n.host.as_ref().is_some_and(|h| {
        h.free() >= needs.boot && h.disk_free >= needs.disk && needs.ceiling.is_none_or(|c| h.mem_total >= c)
    })
}

/// The node a new workspace goes to, from what every node said of itself;
/// `chosen` is an admin's pick, empty for none. A closed, unreachable or
/// other version's node is never one; why not is what people are shown.
pub fn place(nodes: &[NodeView], needs: Needs, chosen: &str) -> Result<String, String> {
    if !chosen.is_empty() {
        let Some(n) = nodes.iter().find(|n| n.name == chosen) else {
            return Err(format!("{chosen:?} isn't a node of this fleet."));
        };
        if !n.takes() {
            return Err(format!("Node {chosen} takes no new workspaces now: it is {:?}.", n.state));
        }
        if !has_room(n, needs) {
            return Err(format!("Node {chosen} has no room for it now."));
        }
        return Ok(chosen.to_owned());
    }
    nodes
        .iter()
        .filter(|n| n.takes() && has_room(n, needs))
        .max_by_key(|n| (n.host.as_ref().map_or(0, workbench_model::fleet::HostInfo::free), std::cmp::Reverse(&n.name)))
        .map(|n| n.name.clone())
        .ok_or_else(|| NO_ROOM.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_model::fleet::{HostInfo, NodeState};

    const GIB: i64 = 1 << 30;

    fn needs(boot_gib: i64, disk_gib: u64) -> Needs {
        Needs { boot: boot_gib * GIB, disk: disk_gib << 30, ceiling: None }
    }

    fn node(name: &str, available_gib: i64, disk_free_gib: u64, state: NodeState) -> NodeView {
        let host = (state == NodeState::Up || state == NodeState::Closed).then(|| HostInfo {
            mem_total: 64 * GIB,
            mem_available: available_gib * GIB,
            reserve: 2 * GIB,
            disk_free: disk_free_gib << 30,
            ..HostInfo::default()
        });
        NodeView { name: name.into(), state, host, detail: String::new() }
    }

    #[test]
    fn the_most_free_memory_wins() {
        let nodes = [node("a", 10, 500, NodeState::Up), node("b", 20, 500, NodeState::Up)];
        assert_eq!(place(&nodes, needs(2, 64), ""), Ok("b".into()));
        // b's disk is full: a.
        let nodes = [node("a", 10, 500, NodeState::Up), node("b", 20, 10, NodeState::Up)];
        assert_eq!(place(&nodes, needs(2, 64), ""), Ok("a".into()));
        // Free memory is what is available less the reserve.
        let tight = [node("a", 3, 500, NodeState::Up)];
        assert_eq!(place(&tight, needs(2, 64), ""), Err(NO_ROOM.into()), "1 GiB free, 2 to boot");
        // A tie goes to the first by name.
        let nodes = [node("b", 10, 500, NodeState::Up), node("a", 10, 500, NodeState::Up)];
        assert_eq!(place(&nodes, needs(2, 64), ""), Ok("a".into()));
    }

    #[test]
    fn closed_unreachable_and_other_versions_take_none() {
        let nodes = [
            node("a", 10, 500, NodeState::Up),
            node("b", 90, 500, NodeState::Closed),
            node("c", 0, 0, NodeState::Unreachable),
            node("d", 0, 0, NodeState::OtherVersion),
        ];
        assert_eq!(place(&nodes, needs(2, 64), ""), Ok("a".into()), "b is closed");
        for chosen in ["b", "c", "d"] {
            assert!(place(&nodes, needs(2, 64), chosen).unwrap_err().contains("takes no new"), "{chosen}");
        }
        assert!(place(&nodes, needs(2, 64), "e").unwrap_err().contains("isn't a node"));
        assert_eq!(place(&nodes[1..], needs(2, 64), ""), Err(NO_ROOM.into()));
    }

    /// An admin's pick goes where it says, if it has room.
    #[test]
    fn an_admin_picks() {
        let nodes = [node("a", 10, 500, NodeState::Up), node("b", 20, 500, NodeState::Up)];
        assert_eq!(place(&nodes, needs(2, 64), "a"), Ok("a".into()));
        assert!(place(&nodes, needs(2, 600), "a").unwrap_err().contains("no room"));
        assert!(place(&nodes, needs(9, 64), "a").unwrap_err().contains("no room"));
    }

    /// A workspace may grow to its node's memory, and no more.
    #[test]
    fn its_most_memory_is_its_node_s() {
        let mut big = node("b", 10, 500, NodeState::Up);
        big.host.as_mut().unwrap().mem_total = 128 * GIB;
        let nodes = [node("a", 20, 500, NodeState::Up), big];
        let grows = Needs { ceiling: Some(100 * GIB), ..needs(2, 64) };
        assert_eq!(place(&nodes, grows, ""), Ok("b".into()), "a has 64 GiB, more free but too little");
        assert!(place(&nodes, grows, "a").unwrap_err().contains("no room"));
        assert_eq!(place(&nodes[..1], Needs { ceiling: Some(200 * GIB), ..grows }, ""), Err(NO_ROOM.into()));
        assert_eq!(place(&nodes, Needs { ceiling: None, ..grows }, ""), Ok("a".into()), "auto is any node's");
    }
}
