use std::collections::{BTreeMap, BTreeSet};

use petgraph::algo::{has_path_connecting, toposort};
use petgraph::dot::{Config, Dot};
use petgraph::graph::{DiGraph, NodeIndex};
use serde_json::Value;
use thiserror::Error;

use crate::library::Library;
use crate::spec::FetchSource;
use crate::spec::{NodeSpec, Reducer, SPEC_VERSION, ToolAccess, WorkflowSpec};

#[derive(Debug, Error)]
pub enum SpecError {
    #[error("workflow yaml is invalid: {0}")]
    Yaml(String),
    #[error("workflow version {found} is not supported, expected {expected}")]
    Version { found: u32, expected: u32 },
    #[error("workflow has no nodes")]
    Empty,
    #[error("{location} names unknown node `{node}`")]
    UnknownNode { location: String, node: String },
    #[error("node `{node}` names unknown channel `{channel}`")]
    UnknownChannel { node: String, channel: String },
    #[error("nodes form a cycle through `{node}`; only a loop node may repeat")]
    Cycle { node: String },
    #[error("{node} is unbounded: {what}")]
    Unbounded { node: String, what: String },
    #[error("expression in {location} is invalid: {message}")]
    Expression { location: String, message: String },
    #[error("expression in {location} uses unknown variable `{variable}`")]
    UnknownVariable { location: String, variable: String },
    #[error("schema of channel `{channel}` is invalid: {message}")]
    Schema { channel: String, message: String },
    #[error("init value of channel `{channel}` violates its schema: {message}")]
    InitInvalid { channel: String, message: String },
    #[error("node `{node}` reads channel `{channel}` that nothing writes and that has no init")]
    UnwrittenRead { node: String, channel: String },
    #[error(
        "node `{node}` reads channel `{channel}` written by `{writer}`, which is not its ancestor"
    )]
    NotAncestorWriter {
        node: String,
        channel: String,
        writer: String,
    },
    #[error(
        "nodes `{first}` and `{second}` run in parallel and both write overwrite channel `{channel}`"
    )]
    ParallelWriters {
        channel: String,
        first: String,
        second: String,
    },
    #[error("writer agent `{agent}` runs in parallel with `{other}`; writes stay single threaded")]
    ParallelWriterAgent { agent: String, other: String },
    #[error("node `{node}` writes verified channel `{channel}`; only a verifier may")]
    VerifiedChannelWrite { node: String, channel: String },
    #[error("verifier `{node}` passes into `{channel}`, which is not a verified channel")]
    VerifierTarget { node: String, channel: String },
    #[error("promote node `{node}` cannot take from `{from}`: {reason}")]
    PromoteSource {
        node: String,
        from: String,
        reason: String,
    },
    #[error("promote node `{node}` requires `{channel}`, which is not a verified channel")]
    RequiresNotVerified { node: String, channel: String },
    #[error("promote nodes `{first}` and `{second}` both promote `{writer}`")]
    DuplicatePromote {
        writer: String,
        first: String,
        second: String,
    },
    #[error("writer `{writer}` has no promote node, so its work could never be merged")]
    UnpromotedWriter { writer: String },
    #[error("publish node `{node}` cannot take from `{from}`: {reason}")]
    PublishSource {
        node: String,
        from: String,
        reason: String,
    },
    #[error("node `{node}` has a bad context: {reason}")]
    Context { node: String, reason: String },
    #[error("node `{node}` uses template `{template}`, which no library holds")]
    TemplateMissing { node: String, template: String },
    #[error("node `{node}` uses template `{template}`, found twice: {first} and {second}")]
    TemplateAmbiguous {
        node: String,
        template: String,
        first: String,
        second: String,
    },
    #[error("node `{node}` uses templates in a cycle: {chain}")]
    TemplateCycle { node: String, chain: String },
    #[error("template `{template}` is invalid: {message}")]
    TemplateInvalid { template: String, message: String },
    #[error(
        "node `{node}` from template `{template}` declares {declared} for channel `{channel}`, but the workflow says {found}"
    )]
    TemplateOutputMismatch {
        node: String,
        template: String,
        channel: String,
        declared: String,
        found: String,
    },
    #[error("loop `{loop_node}` body node `{node}` is not allowed: {reason}")]
    LoopBody {
        loop_node: String,
        node: String,
        reason: String,
    },
}

#[derive(Debug)]
pub struct Workflow {
    spec: WorkflowSpec,
    hash: String,
    graph: DiGraph<String, ()>,
    waves: Vec<Vec<String>>,
    scopes: BTreeMap<String, String>,
}

pub fn load(yaml: &str) -> Result<Workflow, SpecError> {
    load_with(yaml, &Library::default())
}

pub fn load_with(yaml: &str, library: &Library) -> Result<Workflow, SpecError> {
    let resolved = crate::library::resolve(yaml, library)?;
    let spec: WorkflowSpec =
        serde_json::from_value(resolved).map_err(|error| SpecError::Yaml(error.to_string()))?;
    Workflow::new(spec)
}

pub fn json_schema() -> Value {
    schemars::schema_for!(WorkflowSpec).to_value()
}

impl Workflow {
    pub fn new(spec: WorkflowSpec) -> Result<Self, SpecError> {
        if spec.version != SPEC_VERSION {
            return Err(SpecError::Version {
                found: spec.version,
                expected: SPEC_VERSION,
            });
        }
        if spec.nodes.is_empty() {
            return Err(SpecError::Empty);
        }
        check_channels(&spec)?;
        let owners = loop_owners(&spec)?;
        check_references(&spec)?;
        check_bounds(&spec)?;
        check_expressions(&spec)?;
        let (graph, index) = build_graph(&spec, &owners)?;
        let order = toposort(&graph, None).map_err(|cycle| SpecError::Cycle {
            node: graph
                .node_weight(cycle.node_id())
                .cloned()
                .unwrap_or_default(),
        })?;
        check_state_flow(&spec, &owners, &graph, &index)?;
        check_parallelism(&spec, &owners, &graph, &index)?;
        check_context(&spec, &graph, &index)?;
        let promoted = check_promotion(&spec, &graph, &index)?;
        let waves = waves(&graph, &order);
        let scopes = writer_scopes(&spec, &owners, &graph, &index, &waves, &promoted);
        let hash = hash(&spec)?;
        Ok(Self {
            spec,
            hash,
            graph,
            waves,
            scopes,
        })
    }

    #[must_use]
    pub fn spec(&self) -> &WorkflowSpec {
        &self.spec
    }

    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    #[must_use]
    pub fn writer_scope(&self, node: &str) -> Option<&str> {
        self.scopes.get(node).map(String::as_str)
    }

    #[must_use]
    pub fn needs_repository(&self) -> bool {
        self.spec
            .nodes
            .values()
            .any(|node| matches!(node, NodeSpec::Promote(_) | NodeSpec::Publish(_)))
            || !self.scopes.is_empty()
    }

    #[must_use]
    pub fn waves(&self) -> &[Vec<String>] {
        &self.waves
    }

    #[must_use]
    pub fn dot(&self) -> String {
        let labeled = self
            .graph
            .map(|_, name| name.clone(), |_, ()| String::new());
        format!("{}", Dot::with_config(&labeled, &[Config::EdgeNoLabel]))
    }
}

fn hash(spec: &WorkflowSpec) -> Result<String, SpecError> {
    let canonical = serde_json::to_value(spec)
        .map_err(|error| SpecError::Yaml(error.to_string()))?
        .to_string();
    Ok(blake3::hash(canonical.as_bytes()).to_hex().to_string())
}

fn check_channels(spec: &WorkflowSpec) -> Result<(), SpecError> {
    for (name, channel) in &spec.channels {
        let validator =
            jsonschema::validator_for(&channel.schema).map_err(|error| SpecError::Schema {
                channel: name.clone(),
                message: error.to_string(),
            })?;
        if let Some(init) = &channel.init {
            validator
                .validate(init)
                .map_err(|error| SpecError::InitInvalid {
                    channel: name.clone(),
                    message: error.to_string(),
                })?;
        }
    }
    Ok(())
}

fn loop_owners(spec: &WorkflowSpec) -> Result<BTreeMap<String, String>, SpecError> {
    let mut owners = BTreeMap::new();
    for (id, node) in &spec.nodes {
        let NodeSpec::Loop(body) = node else {
            continue;
        };
        for member in &body.body {
            let reject = |reason: &str| SpecError::LoopBody {
                loop_node: id.clone(),
                node: member.clone(),
                reason: reason.to_owned(),
            };
            let Some(inner) = spec.nodes.get(member) else {
                return Err(SpecError::UnknownNode {
                    location: format!("loop `{id}`"),
                    node: member.clone(),
                });
            };
            if matches!(inner, NodeSpec::Loop(_) | NodeSpec::Router(_)) {
                return Err(reject("loops and routers cannot be nested in a loop"));
            }
            if owners.insert(member.clone(), id.clone()).is_some() {
                return Err(reject("a node can belong to one loop only"));
            }
        }
    }
    Ok(owners)
}

fn node_channels(node: &NodeSpec) -> Vec<String> {
    node.reads().iter().cloned().chain(node.writes()).collect()
}

fn check_references(spec: &WorkflowSpec) -> Result<(), SpecError> {
    for (id, node) in &spec.nodes {
        for channel in node_channels(node) {
            if !spec.channels.contains_key(&channel) {
                return Err(SpecError::UnknownChannel {
                    node: id.clone(),
                    channel,
                });
            }
        }
        let is_verifier = matches!(node, NodeSpec::Verifier(_));
        for channel in node.writes() {
            let verified = spec.channels.get(&channel).is_some_and(|c| c.verified);
            if verified && !is_verifier {
                return Err(SpecError::VerifiedChannelWrite {
                    node: id.clone(),
                    channel,
                });
            }
        }
        if let NodeSpec::Verifier(verifier) = node {
            let verified = spec
                .channels
                .get(&verifier.passes)
                .is_some_and(|c| c.verified);
            if !verified {
                return Err(SpecError::VerifierTarget {
                    node: id.clone(),
                    channel: verifier.passes.clone(),
                });
            }
        }
    }
    for edge in &spec.edges {
        for node in [&edge.from, &edge.to] {
            if !spec.nodes.contains_key(node) {
                return Err(SpecError::UnknownNode {
                    location: format!("edge {} -> {}", edge.from, edge.to),
                    node: node.clone(),
                });
            }
        }
    }
    Ok(())
}

fn check_bounds(spec: &WorkflowSpec) -> Result<(), SpecError> {
    for (id, node) in &spec.nodes {
        match node {
            NodeSpec::Agent(agent) if agent.max_turns.is_none() && agent.budget.is_none() => {
                return Err(SpecError::Unbounded {
                    node: format!("agent `{id}`"),
                    what: "declare max_turns or a budget".to_owned(),
                });
            }
            NodeSpec::Loop(body) if body.max_iterations.is_none() => {
                return Err(SpecError::Unbounded {
                    node: format!("loop `{id}`"),
                    what: "declare max_iterations".to_owned(),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

fn check_expression(location: String, source: &str, allowed: &[&str]) -> Result<(), SpecError> {
    let program = cel::Program::compile(source).map_err(|error| SpecError::Expression {
        location: location.clone(),
        message: error.to_string(),
    })?;
    let references = program.references();
    for variable in references.variables() {
        if !allowed.contains(&variable) {
            return Err(SpecError::UnknownVariable {
                location,
                variable: variable.to_owned(),
            });
        }
    }
    Ok(())
}

fn check_expressions(spec: &WorkflowSpec) -> Result<(), SpecError> {
    for (id, node) in &spec.nodes {
        match node {
            NodeSpec::Verifier(node) => check_expression(
                format!("verifier `{id}` pass_when"),
                &node.pass_when,
                &["exit_code", "stdout", "stderr", "channels"],
            )?,
            NodeSpec::Router(node) => {
                check_expression(format!("router `{id}` expr"), &node.expr, &["channels"])?;
            }
            NodeSpec::Loop(node) => check_expression(
                format!("loop `{id}` until"),
                &node.until,
                &["channels", "iteration"],
            )?,
            NodeSpec::Agent(_)
            | NodeSpec::Command(_)
            | NodeSpec::Promote(_)
            | NodeSpec::Publish(_) => {}
        }
    }
    for edge in &spec.edges {
        let Some(guard) = &edge.when else {
            continue;
        };
        let from_router = matches!(spec.nodes.get(&edge.from), Some(NodeSpec::Router(_)));
        let allowed: &[&str] = if from_router {
            &["channels", "label"]
        } else {
            &["channels"]
        };
        check_expression(
            format!("edge {} -> {} when", edge.from, edge.to),
            guard,
            allowed,
        )?;
    }
    Ok(())
}

type Index = BTreeMap<String, NodeIndex>;

fn build_graph(
    spec: &WorkflowSpec,
    owners: &BTreeMap<String, String>,
) -> Result<(DiGraph<String, ()>, Index), SpecError> {
    let mut graph = DiGraph::new();
    let mut index = Index::new();
    for id in spec.nodes.keys().filter(|id| !owners.contains_key(*id)) {
        index.insert(id.clone(), graph.add_node(id.clone()));
    }
    for edge in &spec.edges {
        let (Some(from), Some(to)) = (index.get(&edge.from), index.get(&edge.to)) else {
            let member = if index.contains_key(&edge.from) {
                &edge.to
            } else {
                &edge.from
            };
            return Err(SpecError::LoopBody {
                loop_node: owners.get(member).cloned().unwrap_or_default(),
                node: member.clone(),
                reason: "a loop body node cannot appear in edges".to_owned(),
            });
        };
        graph.add_edge(*from, *to, ());
    }
    Ok((graph, index))
}

fn unit_of<'a>(id: &'a str, owners: &'a BTreeMap<String, String>) -> &'a str {
    owners.get(id).map_or(id, String::as_str)
}

fn is_ancestor(graph: &DiGraph<String, ()>, index: &Index, ancestor: &str, node: &str) -> bool {
    match (index.get(ancestor), index.get(node)) {
        (Some(a), Some(b)) => a != b && has_path_connecting(graph, *a, *b, None),
        _ => false,
    }
}

fn check_state_flow(
    spec: &WorkflowSpec,
    owners: &BTreeMap<String, String>,
    graph: &DiGraph<String, ()>,
    index: &Index,
) -> Result<(), SpecError> {
    let mut writers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (id, node) in &spec.nodes {
        for channel in node.writes() {
            if let Some((name, _)) = spec.channels.get_key_value(&channel) {
                writers
                    .entry(name.as_str())
                    .or_default()
                    .insert(unit_of(id, owners));
            }
        }
    }
    for (id, node) in &spec.nodes {
        let unit = unit_of(id, owners);
        for channel in node.reads() {
            let channel_writers = writers.get(channel.as_str());
            let has_init = spec.channels.get(channel).is_some_and(|c| c.init.is_some());
            let mut ancestor_written = false;
            for writer in channel_writers.into_iter().flatten() {
                if *writer == unit {
                    continue;
                }
                if !is_ancestor(graph, index, writer, unit) {
                    return Err(SpecError::NotAncestorWriter {
                        node: id.clone(),
                        channel: channel.clone(),
                        writer: (*writer).to_owned(),
                    });
                }
                ancestor_written = true;
            }
            if !ancestor_written && !has_init {
                return Err(SpecError::UnwrittenRead {
                    node: id.clone(),
                    channel: channel.clone(),
                });
            }
        }
    }
    Ok(())
}

fn is_writer_agent(spec: &WorkflowSpec, id: &str) -> bool {
    match spec.nodes.get(id) {
        Some(NodeSpec::Agent(agent)) => agent.tools == ToolAccess::Writer,
        Some(NodeSpec::Loop(body)) => body.body.iter().any(|member| is_writer_agent(spec, member)),
        Some(NodeSpec::Promote(_) | NodeSpec::Publish(_)) => true,
        _ => false,
    }
}

fn unit_writes(
    spec: &WorkflowSpec,
    owners: &BTreeMap<String, String>,
    unit: &str,
) -> BTreeSet<String> {
    spec.nodes
        .iter()
        .filter(|(id, _)| unit_of(id, owners) == unit)
        .flat_map(|(_, node)| node.writes())
        .collect()
}

fn check_parallelism(
    spec: &WorkflowSpec,
    owners: &BTreeMap<String, String>,
    graph: &DiGraph<String, ()>,
    index: &Index,
) -> Result<(), SpecError> {
    let units: Vec<&String> = index.keys().collect();
    for (position, first) in units.iter().enumerate() {
        for second in units.iter().skip(position + 1) {
            if is_ancestor(graph, index, first, second) || is_ancestor(graph, index, second, first)
            {
                continue;
            }
            for (writer, other) in [(first, second), (second, first)] {
                if is_writer_agent(spec, writer) {
                    return Err(SpecError::ParallelWriterAgent {
                        agent: (*writer).clone(),
                        other: (*other).clone(),
                    });
                }
            }
            let shared: Vec<String> = unit_writes(spec, owners, first)
                .intersection(&unit_writes(spec, owners, second))
                .cloned()
                .collect();
            for channel in shared {
                let overwrite = spec
                    .channels
                    .get(&channel)
                    .is_some_and(|c| c.reducer == Reducer::Overwrite);
                if overwrite {
                    return Err(SpecError::ParallelWriters {
                        channel,
                        first: (*first).clone(),
                        second: (*second).clone(),
                    });
                }
            }
        }
    }
    Ok(())
}

fn waves(graph: &DiGraph<String, ()>, order: &[NodeIndex]) -> Vec<Vec<String>> {
    let mut level: BTreeMap<NodeIndex, usize> = BTreeMap::new();
    let mut waves: Vec<Vec<String>> = Vec::new();
    for node in order {
        let depth = graph
            .neighbors_directed(*node, petgraph::Direction::Incoming)
            .filter_map(|parent| level.get(&parent))
            .max()
            .map_or(0, |deepest| deepest + 1);
        level.insert(*node, depth);
        while waves.len() <= depth {
            waves.push(Vec::new());
        }
        if let (Some(wave), Some(name)) = (waves.get_mut(depth), graph.node_weight(*node)) {
            wave.push(name.clone());
        }
    }
    for wave in &mut waves {
        wave.sort();
    }
    waves
}

fn holds_writer_agent(spec: &WorkflowSpec, id: &str) -> bool {
    match spec.nodes.get(id) {
        Some(NodeSpec::Agent(agent)) => agent.tools == ToolAccess::Writer,
        Some(NodeSpec::Loop(body)) => body
            .body
            .iter()
            .any(|member| holds_writer_agent(spec, member)),
        _ => false,
    }
}

fn check_promotion(
    spec: &WorkflowSpec,
    graph: &DiGraph<String, ()>,
    index: &Index,
) -> Result<BTreeMap<String, String>, SpecError> {
    let mut promoted: BTreeMap<&str, &str> = BTreeMap::new();
    for (id, node) in &spec.nodes {
        let NodeSpec::Promote(promote) = node else {
            continue;
        };
        let reject = |reason: &str| SpecError::PromoteSource {
            node: id.clone(),
            from: promote.from.clone(),
            reason: reason.to_owned(),
        };
        if !index.contains_key(&promote.from) {
            return Err(reject("it is not a node of the graph"));
        }
        if !holds_writer_agent(spec, &promote.from) {
            return Err(reject("it is not a writer agent or a loop holding one"));
        }
        if !is_ancestor(graph, index, &promote.from, id) {
            return Err(reject("it does not run before the promote node"));
        }
        for channel in &promote.requires {
            if !spec.channels.get(channel).is_some_and(|c| c.verified) {
                return Err(SpecError::RequiresNotVerified {
                    node: id.clone(),
                    channel: channel.clone(),
                });
            }
        }
        if let Some(first) = promoted.insert(&promote.from, id) {
            return Err(SpecError::DuplicatePromote {
                writer: promote.from.clone(),
                first: first.to_owned(),
                second: id.clone(),
            });
        }
    }
    for id in index.keys() {
        if holds_writer_agent(spec, id) && !promoted.contains_key(id.as_str()) {
            return Err(SpecError::UnpromotedWriter { writer: id.clone() });
        }
    }
    for (id, node) in &spec.nodes {
        let NodeSpec::Publish(publish) = node else {
            continue;
        };
        let reject = |reason: &str| SpecError::PublishSource {
            node: id.clone(),
            from: publish.from.clone(),
            reason: reason.to_owned(),
        };
        if !matches!(spec.nodes.get(&publish.from), Some(NodeSpec::Promote(_))) {
            return Err(reject("it is not a promote node"));
        }
        if !is_ancestor(graph, index, &publish.from, id) {
            return Err(reject("it does not run before the publish node"));
        }
        if reachable_without(graph, index, &publish.from, id) {
            return Err(reject(
                "a branch can reach the publish node without passing through it",
            ));
        }
    }
    Ok(promoted
        .into_iter()
        .map(|(writer, promote)| (writer.to_owned(), promote.to_owned()))
        .collect())
}

fn writer_scopes(
    spec: &WorkflowSpec,
    owners: &BTreeMap<String, String>,
    graph: &DiGraph<String, ()>,
    index: &Index,
    waves: &[Vec<String>],
    promoted: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let depth: BTreeMap<&str, usize> = waves
        .iter()
        .enumerate()
        .flat_map(|(level, wave)| wave.iter().map(move |node| (node.as_str(), level)))
        .collect();
    let mut scopes = BTreeMap::new();
    for id in spec.nodes.keys() {
        let unit = unit_of(id, owners);
        let chosen = index
            .keys()
            .filter(|writer| holds_writer_agent(spec, writer))
            .filter(|writer| writer.as_str() == unit || is_ancestor(graph, index, writer, unit))
            .filter(|writer| {
                promoted
                    .get(writer.as_str())
                    .is_none_or(|promote| !is_ancestor(graph, index, promote, unit))
            })
            .max_by_key(|writer| depth.get(writer.as_str()).copied().unwrap_or_default());
        if let Some(writer) = chosen {
            scopes.insert(id.clone(), writer.clone());
        }
    }
    scopes
}

fn reachable_without(
    graph: &DiGraph<String, ()>,
    index: &Index,
    blocked: &str,
    target: &str,
) -> bool {
    let (Some(blocked), Some(target)) = (index.get(blocked), index.get(target)) else {
        return false;
    };
    let mut seen = BTreeSet::new();
    let mut stack: Vec<NodeIndex> = graph
        .externals(petgraph::Direction::Incoming)
        .filter(|root| root != blocked)
        .collect();
    while let Some(node) = stack.pop() {
        if !seen.insert(node) {
            continue;
        }
        if node == *target {
            return true;
        }
        stack.extend(
            graph
                .neighbors_directed(node, petgraph::Direction::Outgoing)
                .filter(|next| next != blocked),
        );
    }
    false
}

fn check_context(
    spec: &WorkflowSpec,
    graph: &DiGraph<String, ()>,
    index: &Index,
) -> Result<(), SpecError> {
    for (id, node) in &spec.nodes {
        let NodeSpec::Agent(agent) = node else {
            continue;
        };
        let bad = |reason: String| SpecError::Context {
            node: id.clone(),
            reason,
        };
        for upstream in &agent.context.from_nodes {
            if !index.contains_key(upstream) || upstream == id {
                return Err(bad(format!(
                    "from_nodes names `{upstream}`, which is not another node"
                )));
            }
            if !is_ancestor(graph, index, upstream, id) {
                return Err(bad(format!(
                    "from_nodes names `{upstream}`, which does not run before it"
                )));
            }
        }
        for fetch in &agent.context.fetch {
            let empty = match &fetch.source {
                FetchSource::Treesitter { paths, .. } | FetchSource::Lsp { paths, .. } => {
                    paths.is_empty()
                }
                FetchSource::Files { globs } => globs.is_empty(),
                FetchSource::Skill { name } => name.is_empty(),
                FetchSource::Mcp { resource, tool, .. } => resource.is_none() == tool.is_none(),
            };
            if empty {
                return Err(bad("a fetch source is missing what to fetch (paths, globs, a name, or exactly one of resource and tool)".to_owned()));
            }
        }
        for fetch in &agent.context.fetch {
            if let FetchSource::Mcp { server, .. } = &fetch.source
                && !agent.mcp.contains_key(server)
            {
                return Err(bad(format!(
                    "a fetch source uses mcp server `{server}`, which the node does not attach"
                )));
            }
        }
        for name in agent.skills.iter().chain(agent.mcp.keys()) {
            if name.is_empty() {
                return Err(bad(
                    "an attached skill or server has an empty name".to_owned()
                ));
            }
        }
    }
    Ok(())
}
