use std::path::Path;

use jensen_harness_workflow::library::Library;
use jensen_harness_workflow::{NodeSpec, SpecError, load_with};

fn library(dir: &Path) -> Library {
    Library::new(vec![dir.to_path_buf()])
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(format!("{name}.yaml")), text).unwrap();
}

const FINDER: &str = r#"
description: finds things
role: explore
prompt: Find things.
tools: read_only
max_turns: 6
skills: [search]
mcp: [figma]
inputs:
  task: {}
outputs:
  findings:
    schema: { type: array, items: { type: string } }
"#;

const WORKFLOW: &str = r#"
version: 1
name: t
channels:
  task: { schema: { type: string }, init: "" }
  findings: { schema: { type: array, items: { type: string } }, reducer: append }
nodes:
  find:
    kind: agent
    uses: finder
    max_turns: 9
    skills: [extra]
    mcp:
      excalidraw: { tools: [draw] }
"#;

fn agent(
    workflow: &jensen_harness_workflow::Workflow,
    id: &str,
) -> jensen_harness_workflow::spec::AgentNode {
    match workflow.spec().nodes.get(id).unwrap() {
        NodeSpec::Agent(agent) => agent.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_template_is_merged_under_the_node_with_lists_appended_and_scalars_overridden() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "finder", FINDER);

    let workflow = load_with(WORKFLOW, &library(dir.path())).unwrap();

    let node = agent(&workflow, "find");
    assert_eq!(node.role, "explore");
    assert_eq!(node.max_turns, Some(9), "the node overrides a scalar");
    assert_eq!(node.skills, ["search", "extra"], "lists append");
    assert_eq!(
        node.mcp.keys().map(String::as_str).collect::<Vec<_>>(),
        ["excalidraw", "figma"],
        "maps merge, and a list of names is a map"
    );
    assert_eq!(node.mcp["excalidraw"].tools, ["draw"]);
    assert_eq!(node.reads, ["task"], "inputs become reads");
    assert_eq!(node.writes, ["findings"], "outputs become writes");
}

#[test]
fn a_template_that_is_missing_ambiguous_or_cyclic_is_refused_naming_the_node() {
    let dir = tempfile::tempdir().unwrap();
    let error = load_with(WORKFLOW, &library(dir.path())).unwrap_err();
    assert!(
        matches!(error, SpecError::TemplateMissing { node, template } if node == "find" && template == "finder")
    );

    let other = tempfile::tempdir().unwrap();
    write(dir.path(), "finder", FINDER);
    write(other.path(), "finder", FINDER);
    let both = Library::new(vec![dir.path().to_path_buf(), other.path().to_path_buf()]);
    assert!(matches!(
        load_with(WORKFLOW, &both).unwrap_err(),
        SpecError::TemplateAmbiguous { template, .. } if template == "finder"
    ));

    write(dir.path(), "a", "uses: b\nrole: r\nprompt: p\n");
    write(dir.path(), "b", "uses: a\nrole: r\nprompt: p\n");
    let cyclic = WORKFLOW.replace("uses: finder", "uses: a");
    assert!(matches!(
        load_with(&cyclic, &library(dir.path())).unwrap_err(),
        SpecError::TemplateCycle { chain, .. } if chain.contains("a -> b -> a")
    ));
}

#[test]
fn a_template_can_build_on_another() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "finder", FINDER);
    write(
        dir.path(),
        "careful",
        "uses: finder\nmax_turns: 3\nprompt: Find things carefully.\n",
    );
    let yaml = WORKFLOW
        .replace("uses: finder", "uses: careful")
        .replace("    max_turns: 9\n", "");

    let node = agent(&load_with(&yaml, &library(dir.path())).unwrap(), "find");

    assert_eq!(
        (node.max_turns, node.prompt.as_str()),
        (Some(3), "Find things carefully.")
    );
    assert_eq!(node.skills, ["search", "extra"]);
}

#[test]
fn an_output_schema_that_does_not_fit_the_channel_is_refused_naming_both() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "finder", FINDER);
    let yaml = WORKFLOW.replace(
        "findings: { schema: { type: array, items: { type: string } }, reducer: append }",
        "findings: { schema: { type: array, items: { type: number } }, reducer: append }",
    );
    let error = load_with(&yaml, &library(dir.path())).unwrap_err();
    assert!(
        matches!(
            &error,
            SpecError::TemplateOutputMismatch { template, channel, .. } if template == "finder" && channel == "findings"
        ),
        "{error}"
    );
}

#[test]
fn only_agents_use_templates_and_names_are_plain() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "finder", FINDER);
    let command = "version: 1\nname: t\nnodes:\n  c: {kind: command, run: x, uses: finder}\n";
    assert!(matches!(
        load_with(command, &library(dir.path())).unwrap_err(),
        SpecError::TemplateInvalid { .. }
    ));
    let sneaky = WORKFLOW.replace("uses: finder", "uses: ../finder");
    assert!(matches!(
        load_with(&sneaky, &library(dir.path())).unwrap_err(),
        SpecError::TemplateInvalid { .. }
    ));
}

#[test]
fn the_hash_changes_when_the_template_changes() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "finder", FINDER);
    let first = load_with(WORKFLOW, &library(dir.path()))
        .unwrap()
        .hash()
        .to_owned();
    write(
        dir.path(),
        "finder",
        &FINDER.replace("Find things.", "Find other things."),
    );
    let second = load_with(WORKFLOW, &library(dir.path()))
        .unwrap()
        .hash()
        .to_owned();
    assert_ne!(first, second);
}

#[test]
fn context_rules_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "finder", FINDER);
    let upstream = "version: 1\nname: t\nchannels:\n  task: {schema: {type: string}, init: ''}\nnodes:\n  a: {kind: command, run: x}\n  b: {kind: agent, role: r, prompt: p, max_turns: 3, context: {from_nodes: [a]}}\n";
    assert!(matches!(
        load_with(upstream, &library(dir.path())).unwrap_err(),
        SpecError::Context { node, .. } if node == "b"
    ));
    let ordered = format!("{upstream}edges:\n  - {{ from: a, to: b }}\n");
    load_with(&ordered, &library(dir.path())).unwrap();
    let empty_fetch = ordered.replace("from_nodes: [a]", "fetch: [{source: files, globs: []}]");
    assert!(matches!(
        load_with(&empty_fetch, &library(dir.path())).unwrap_err(),
        SpecError::Context { .. }
    ));
    let inherited = ordered.replace(
        "from_nodes: [a]",
        "inherit: [{channel: task, mode: summary}]",
    );
    let workflow = load_with(&inherited, &library(dir.path())).unwrap();
    assert_eq!(agent(&workflow, "b").reads, ["task"]);
}

#[test]
fn the_shipped_library_loads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("agents");
    let yaml = "version: 1\nname: t\nchannels:\n  task: {schema: {type: string}, init: ''}\n  findings: {schema: {type: array, items: {type: string}}, reducer: append}\nnodes:\n  r: {kind: agent, uses: reviewer}\n";
    let workflow = load_with(yaml, &Library::new(vec![root])).unwrap();
    assert_eq!(agent(&workflow, "r").role, "review");
}
