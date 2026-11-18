//! Compact native sidebar primitives. Brand assets retain their upstream notices.
use gpui::{prelude::*, *};
use gpui_component::tooltip::Tooltip;
use std::borrow::Cow;

pub struct Assets;
impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let bytes: &'static [u8] = match path {
            "cli/codex.svg" => include_bytes!("../assets/cli-icons/codex.svg"),
            "cli/claude.svg" => include_bytes!("../assets/cli-icons/claude.svg"),
            "cli/pi.svg" => include_bytes!("../assets/cli-icons/pi.svg"),
            "cli/agy.svg" => include_bytes!("../assets/cli-icons/antigravity.svg"),
            "cli/custom.svg" => include_bytes!("../assets/cli-icons/agent.svg"),
            _ => return Ok(None),
        };
        Ok(Some(Cow::Borrowed(bytes)))
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(vec![])
    }
}

pub fn cli_label(agent: &str) -> &'static str {
    match agent {
        "codex" => "Codex",
        "claude" => "Claude Code",
        "pi" => "Pi",
        "agy" | "antigravity" => "Antigravity",
        _ => "Custom CLI",
    }
}

pub fn cli_icon(agent: &str, id: String) -> Stateful<Div> {
    let path = match agent {
        "codex" => "cli/codex.svg",
        "claude" => "cli/claude.svg",
        "pi" => "cli/pi.svg",
        "agy" | "antigravity" => "cli/agy.svg",
        _ => "cli/custom.svg",
    };
    let label = cli_label(agent);
    div()
        .id(SharedString::from(id))
        .size(px(18.))
        .when(agent == "pi", |icon| icon.rounded(px(4.)).bg(rgb(0x343440)))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
        .child(img(path).size(px(if agent == "pi" { 14. } else { 16. })))
}

pub fn status_dot(status: &str, label: String, id: String) -> Stateful<Div> {
    let color = match status {
        "busy" | "running" | "working" => 0x4f46e5,
        "error" | "offline" | "needs_attention" | "failed" => 0xc44e4e,
        "blocked" | "paused" | "taken_over" | "pending" => 0xb88a36,
        "done" | "ready" | "completed" => 0x4f8870,
        "aborted" => 0x70707c,
        _ => 0x96969e,
    };
    let ring = matches!(
        status,
        "idle" | "sleeping" | "unknown" | "pending" | "aborted"
    );
    div()
        .id(SharedString::from(id))
        .size(px(16.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
        .child(
            div()
                .size(px(7.))
                .rounded_full()
                .border_1()
                .border_color(rgb(color))
                .when(!ring, |dot| dot.bg(rgb(color)))
                .when(status == "aborted", |dot| {
                    dot.flex()
                        .items_center()
                        .justify_center()
                        .child(div().w(px(3.)).h(px(1.)).bg(rgb(color)))
                }),
        )
}

use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Deserialize)]
pub struct Node {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub cli: String,
    pub state: String,
    #[serde(default)]
    pub observation: String,
    #[serde(default)]
    pub target: Option<Value>,
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub status: String,
}

pub struct Row<'a> {
    pub node: &'a Node,
    pub branches: Vec<bool>,
    pub last: bool,
    pub has_children: bool,
}

/// A current diagnostic repair Agent observes its own children while the
/// project's resumable conversation remains immutable. Never mix A's cached
/// descendants into B's repair lane, or retain B after that role is retired.
pub fn observed_native(project: &Value) -> &str {
    let saved = project["native_session"].as_str().unwrap_or("");
    let lane = &project["execution"]["repair_lane"];
    if !saved.is_empty()
        && lane["generation"] == project["generation"]
        && project["generation"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
        && lane["expected_native"] == saved
        && let Some(observed) = lane["observed_native"].as_str().filter(|s| !s.is_empty())
    {
        observed
    } else {
        saved
    }
}

impl Tree {
    pub fn from_project(project: &Value) -> Self {
        let value = &project["execution"]["agent_tree"];
        let scoped = value["project_id"] == project["id"]
            && value["generation"] == project["generation"]
            && value["native_session"] == observed_native(project)
            && (value.get("project_native_session").is_none()
                || value["project_native_session"] == project["native_session"])
            && project["generation"]
                .as_str()
                .is_some_and(|s| !s.is_empty());
        if !scoped {
            return Self {
                nodes: vec![],
                status: "unknown".into(),
            };
        }
        let mut status = value["status"].as_str().unwrap_or("unknown").to_owned();
        let mut nodes = Vec::new();
        let mut ids = HashSet::new();
        for value in value["nodes"].as_array().into_iter().flatten().take(256) {
            let Ok(node) = serde_json::from_value::<Node>(value.clone()) else {
                status = "partial".into();
                continue;
            };
            if node.id.is_empty() || node.name.is_empty() || !ids.insert(node.id.clone()) {
                // Ambiguous identity must not acquire a click target or ancestry.
                return Self {
                    nodes: vec![],
                    status: "partial".into(),
                };
            }
            nodes.push(node);
        }
        if value["nodes"]
            .as_array()
            .is_some_and(|nodes| nodes.len() > 256)
        {
            status = "partial".into();
        }
        // The project row already represents the explicit main Agent root.
        if let Some(root) = nodes
            .iter()
            .position(|node| node.id == "root" && node.parent_id.is_none())
        {
            nodes.remove(root);
            for node in &mut nodes {
                if node.parent_id.as_deref() == Some("root") {
                    node.parent_id = None;
                }
            }
        }
        let mut tree = Self { nodes, status };
        if tree.rows(&HashSet::new()).len() != tree.nodes.len() {
            tree.status = "partial".into();
        }
        tree
    }

    pub fn rows(&self, folded: &HashSet<String>) -> Vec<Row<'_>> {
        let mut children: HashMap<Option<&str>, Vec<&Node>> = HashMap::new();
        for node in &self.nodes {
            children
                .entry(node.parent_id.as_deref())
                .or_default()
                .push(node);
        }
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        Self::append(&children, None, &[], folded, &mut seen, &mut rows);
        rows
    }

    fn append<'a>(
        children: &HashMap<Option<&str>, Vec<&'a Node>>,
        parent: Option<&str>,
        branches: &[bool],
        folded: &HashSet<String>,
        seen: &mut HashSet<&'a str>,
        rows: &mut Vec<Row<'a>>,
    ) {
        let Some(kids) = children.get(&parent) else {
            return;
        };
        for (index, node) in kids.iter().enumerate() {
            if !seen.insert(&node.id) {
                continue;
            }
            let last = index + 1 == kids.len();
            let has_children = children.contains_key(&Some(node.id.as_str()));
            rows.push(Row {
                node,
                branches: branches.to_vec(),
                last,
                has_children,
            });
            if !folded.contains(&node.id) {
                let mut next = branches.to_vec();
                next.push(!last);
                Self::append(children, Some(&node.id), &next, folded, seen, rows);
            }
        }
    }
}

/// Only an explicit existing project identity can be navigated. A discovered
/// native child without its own project terminal stays a read-only observation.
pub fn project_target(node: &Node, projects: &Value) -> Option<String> {
    let target = node.target.as_ref()?;
    projects.as_array()?.iter().find(|project| {
        target["project_id"] == project["id"]
            && target["generation"] == project["generation"]
            && target["native_session"] == project["native_session"]
            && target["generation"].as_str().is_some_and(|s| !s.is_empty())
    })?["id"]
        .as_str()
        .map(str::to_owned)
}

/// Continuation lines follow the reference's depth-first rows. Geometry is
/// native GPUI borders, so indentation does not depend on font box glyphs.
pub fn branches(row: &Row<'_>) -> Div {
    let mut lines = div().flex().h_full().items_center().flex_shrink_0();
    if row.branches.len() > 3 {
        lines = lines.child(div().w(px(12.)).text_size(px(10.)).child("…"));
    }
    for continuing in row
        .branches
        .iter()
        .skip(row.branches.len().saturating_sub(3))
    {
        lines = lines.child(div().w(px(12.)).h_full().when(*continuing, |line| {
            line.border_l_1().border_color(rgb(0xd7d7df))
        }));
    }
    lines.child(
        div()
            .w(px(12.))
            .h_full()
            .relative()
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .h(px(13.))
                    .w(px(8.))
                    .border_l_1()
                    .border_b_1()
                    .border_color(rgb(0xd7d7df))
                    .rounded_bl(px(4.)),
            )
            .when(!row.last, |line| {
                line.child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(13.))
                        .bottom_0()
                        .border_l_1()
                        .border_color(rgb(0xd7d7df)),
                )
            }),
    )
}

#[cfg(test)]
mod tests {
    use super::{Node, Tree, project_target};
    use serde_json::{Value, json};
    use std::collections::HashSet;

    #[test]
    fn preserves_nested_identity_and_folded_sibling_branches() {
        let project = json!({"id":"p","generation":"g","native_session":"a","execution":{"agent_tree":{
        "project_id":"p","generation":"g","native_session":"a","status":"partial","nodes":[
            {"id":"root","parent_id":null,"name":"Project","cli":"codex","state":"unknown"},
            {"id":"one","parent_id":"root","name":"First","cli":"codex","state":"working"},
            {"id":"nested","parent_id":"one","name":"Nested","cli":"codex","state":"idle"},
            {"id":"two","parent_id":"root","name":"Second","cli":"pi","state":"idle"},
            {"id":"orphan","parent_id":"missing","name":"Unlinked","cli":"pi","state":"unknown"}
        ]}}});
        let tree = Tree::from_project(&project);
        let rows = tree.rows(&HashSet::new());
        assert_eq!(
            rows.iter().map(|r| r.node.id.as_str()).collect::<Vec<_>>(),
            ["one", "nested", "two"]
        );
        assert_eq!(rows[1].branches, [true]);
        assert!(!rows[0].last && rows[2].last);
        let folded = tree.rows(&HashSet::from(["one".into()]));
        assert_eq!(
            folded
                .iter()
                .map(|r| r.node.id.as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
        let mut stale = project;
        stale["generation"] = json!("new");
        assert!(Tree::from_project(&stale).nodes.is_empty());
    }

    #[test]
    fn duplicate_or_cyclic_observations_never_invent_a_root_or_navigation_target() {
        let mut project = json!({"id":"p","generation":"g","native_session":"a","execution":{"agent_tree":{
        "project_id":"p","generation":"g","native_session":"a","status":"observed","nodes":[
            {"id":"one","parent_id":"two","name":"First","cli":"codex","state":"unknown"},
            {"id":"two","parent_id":"one","name":"Second","cli":"codex","state":"unknown"}
        ]}}});
        let tree = Tree::from_project(&project);
        assert_eq!(tree.status, "partial");
        assert!(tree.rows(&HashSet::new()).is_empty());
        project["execution"]["agent_tree"]["nodes"][1]["id"] = json!("one");
        assert!(Tree::from_project(&project).nodes.is_empty());
        let node: Node = serde_json::from_value(
            json!({"id":"kid","parent_id":null,"name":"Child","cli":"pi","state":"working",
            "target":{"project_id":"other","generation":"old","native_session":"saved"}}),
        )
        .unwrap();
        let projects = json!([{"id":"other","generation":"current","native_session":"saved"}]);
        assert!(project_target(&node, &projects).is_none());
    }

    #[test]
    fn repair_children_follow_current_b_without_adopting_b_or_leaking_stale_a() {
        let mut project = json!({"id":"p","generation":"g","native_session":"a","execution":{"agent_tree":{
            "project_id":"p","generation":"g","native_session":"a","project_native_session":"a","status":"partial",
            "nodes":[{"id":"child","parent_id":null,"name":"Observed child","cli":"codex","state":"unknown"}]
        }}});
        assert_eq!(Tree::from_project(&project).nodes.len(), 1);
        project["execution"]["repair_lane"] =
            json!({"generation":"g","expected_native":"a","observed_native":"b"});
        assert!(
            Tree::from_project(&project).nodes.is_empty(),
            "Cached A children must disappear while B owns the repair view"
        );
        project["execution"]["agent_tree"]["native_session"] = json!("b");
        assert_eq!(Tree::from_project(&project).nodes.len(), 1);
        assert_eq!(project["native_session"], "a");
        project["execution"]["repair_lane"]["generation"] = json!("old");
        assert!(Tree::from_project(&project).nodes.is_empty());
        project["execution"]["repair_lane"]["generation"] = json!("g");
        project["execution"]["repair_lane"]["expected_native"] = json!("unrelated");
        assert!(Tree::from_project(&project).nodes.is_empty());
        project["execution"]["repair_lane"] = Value::Null;
        assert!(
            Tree::from_project(&project).nodes.is_empty(),
            "B's view expires with its repair role"
        );
        project["execution"]["agent_tree"]["native_session"] = json!("a");
        assert_eq!(Tree::from_project(&project).nodes.len(), 1);
    }
}
