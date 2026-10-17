use jensen_harness_workflow::{SpecError, load};

const TRIAGE: &str = include_str!("../../workflows/examples/triage.yaml");

const HEADER: &str = "version: 1\nname: t\n";

fn refuse(yaml: &str) -> SpecError {
    match load(yaml) {
        Ok(_) => panic!("spec was accepted"),
        Err(error) => error,
    }
}

#[test]
fn example_loads_with_waves_and_stable_hash() {
    let first = load(TRIAGE).expect("example loads");
    let second = load(TRIAGE).expect("example loads");
    assert_eq!(first.hash(), second.hash());
    assert_eq!(
        first.waves(),
        &[
            vec!["inspect_code".to_owned(), "inspect_docs".to_owned()],
            vec!["check".to_owned()],
            vec!["route".to_owned()],
        ]
    );
    assert!(first.dot().contains("inspect_code"));
}

#[test]
fn hash_changes_with_the_spec() {
    let changed = TRIAGE.replace("max_turns: 8", "max_turns: 9");
    assert_ne!(
        load(TRIAGE).expect("loads").hash(),
        load(&changed).expect("loads").hash()
    );
}

#[test]
fn json_schema_describes_nodes() {
    let schema = jensen_harness_workflow::json_schema().to_string();
    assert!(schema.contains("verifier"));
}

#[test]
fn unknown_fields_are_refused() {
    let yaml = format!("{HEADER}typo: 1\nnodes: {{}}\n");
    assert!(matches!(refuse(&yaml), SpecError::Yaml(_)));
}

#[test]
fn cycle_is_refused() {
    let yaml = format!(
        "{HEADER}nodes:\n  a: {{kind: command, run: x}}\n  b: {{kind: command, run: x}}\nedges:\n  - {{from: a, to: b}}\n  - {{from: b, to: a}}\n"
    );
    assert!(matches!(refuse(&yaml), SpecError::Cycle { .. }));
}

#[test]
fn unbounded_agent_and_loop_are_refused() {
    let agent = format!("{HEADER}nodes:\n  a: {{kind: agent, role: r, prompt: p}}\n");
    assert!(matches!(refuse(&agent), SpecError::Unbounded { node, .. } if node.contains('a')));
    let looped = format!(
        "{HEADER}nodes:\n  c: {{kind: command, run: x}}\n  l: {{kind: loop, body: [c], until: 'true'}}\n"
    );
    assert!(matches!(refuse(&looped), SpecError::Unbounded { node, .. } if node.contains("loop")));
}

#[test]
fn bad_expression_and_unknown_variable_are_refused() {
    let syntax = format!("{HEADER}nodes:\n  r: {{kind: router, expr: '1 +'}}\n");
    let e = refuse(&syntax);
    assert!(matches!(e, SpecError::Expression { .. }), "{e:?}");
    let variable = format!("{HEADER}nodes:\n  r: {{kind: router, expr: 'secrets.x'}}\n");
    assert!(
        matches!(refuse(&variable), SpecError::UnknownVariable { variable, .. } if variable == "secrets")
    );
}

#[test]
fn bad_schema_and_bad_init_are_refused() {
    let schema = format!(
        "{HEADER}channels:\n  c: {{schema: {{type: 7}}}}\nnodes:\n  a: {{kind: command, run: x}}\n"
    );
    assert!(matches!(refuse(&schema), SpecError::Schema { channel, .. } if channel == "c"));
    let init = format!(
        "{HEADER}channels:\n  c: {{schema: {{type: string}}, init: 3}}\nnodes:\n  a: {{kind: command, run: x}}\n"
    );
    assert!(matches!(refuse(&init), SpecError::InitInvalid { channel, .. } if channel == "c"));
}

#[test]
fn read_before_write_rules() {
    let unwritten = format!(
        "{HEADER}channels:\n  c: {{schema: {{type: string}}}}\nnodes:\n  a: {{kind: command, run: x, reads: [c]}}\n"
    );
    assert!(matches!(refuse(&unwritten), SpecError::UnwrittenRead { node, .. } if node == "a"));
    let not_ancestor = format!(
        "{HEADER}channels:\n  c: {{schema: {{type: string}}, reducer: append}}\nnodes:\n  w: {{kind: command, run: x, writes: [c]}}\n  r: {{kind: command, run: x, reads: [c]}}\n"
    );
    assert!(matches!(
        refuse(&not_ancestor),
        SpecError::NotAncestorWriter { node, writer, .. } if node == "r" && writer == "w"
    ));
}

#[test]
fn parallel_overwrite_writers_are_refused() {
    let yaml = format!(
        "{HEADER}channels:\n  c: {{schema: {{type: string}}}}\nnodes:\n  a: {{kind: command, run: x, writes: [c]}}\n  b: {{kind: command, run: x, writes: [c]}}\n"
    );
    assert!(matches!(refuse(&yaml), SpecError::ParallelWriters { channel, .. } if channel == "c"));
}

#[test]
fn parallel_writer_agent_is_refused() {
    let yaml = format!(
        "{HEADER}nodes:\n  w: {{kind: agent, role: r, prompt: p, tools: writer, max_turns: 3}}\n  o: {{kind: command, run: x}}\n"
    );
    assert!(matches!(
        refuse(&yaml),
        SpecError::ParallelWriterAgent { .. }
    ));
}

#[test]
fn verified_channels_belong_to_verifiers() {
    let write = format!(
        "{HEADER}channels:\n  v: {{schema: {{type: boolean}}, verified: true}}\nnodes:\n  a: {{kind: command, run: x, writes: [v]}}\n"
    );
    assert!(matches!(refuse(&write), SpecError::VerifiedChannelWrite { node, .. } if node == "a"));
    let target = format!(
        "{HEADER}channels:\n  v: {{schema: {{type: boolean}}}}\nnodes:\n  a: {{kind: verifier, run: x, pass_when: 'true', passes: v}}\n"
    );
    assert!(matches!(refuse(&target), SpecError::VerifierTarget { node, .. } if node == "a"));
}

#[test]
fn loop_body_rules() {
    let in_edges = format!(
        "{HEADER}nodes:\n  c: {{kind: command, run: x}}\n  l: {{kind: loop, body: [c], until: 'true', max_iterations: 3}}\nedges:\n  - {{from: c, to: l}}\n"
    );
    assert!(matches!(refuse(&in_edges), SpecError::LoopBody { .. }));
    let ok = format!(
        "{HEADER}nodes:\n  c: {{kind: command, run: x}}\n  l: {{kind: loop, body: [c], until: 'iteration >= 2', max_iterations: 3}}\n"
    );
    load(&ok).expect("bounded loop loads");
}

#[test]
fn unknown_references_are_refused() {
    let edge = format!(
        "{HEADER}nodes:\n  a: {{kind: command, run: x}}\nedges:\n  - {{from: a, to: ghost}}\n"
    );
    assert!(matches!(refuse(&edge), SpecError::UnknownNode { node, .. } if node == "ghost"));
    let channel = format!("{HEADER}nodes:\n  a: {{kind: command, run: x, reads: [ghost]}}\n");
    assert!(
        matches!(refuse(&channel), SpecError::UnknownChannel { channel, .. } if channel == "ghost")
    );
}

const IMPLEMENT: &str = include_str!("../../workflows/examples/implement.yaml");

fn implement_with(from: &str, to: &str) -> String {
    IMPLEMENT.replacen(from, to, 1)
}

#[test]
fn the_implement_example_loads_in_four_waves() {
    let workflow = load(IMPLEMENT).expect("example loads");
    assert_eq!(workflow.waves().len(), 4);
}

#[test]
fn promotion_rules_are_enforced() {
    let read_only = implement_with("    tools: writer\n", "");
    assert!(matches!(
        refuse(&read_only),
        SpecError::PromoteSource { node, .. } if node == "promote"
    ));

    let unverified = implement_with("requires: [checked]", "requires: [task]");
    assert!(matches!(
        refuse(&unverified),
        SpecError::RequiresNotVerified { channel, .. } if channel == "task"
    ));

    let ghost = implement_with("from: write", "from: ghost");
    assert!(matches!(refuse(&ghost), SpecError::PromoteSource { .. }));

    let duplicate = IMPLEMENT.replace(
        "  publish:",
        "  promote_again:\n    kind: promote\n    from: write\n    requires: [checked]\n  publish:",
    )
    .replace(
        "  - { from: promote, to: publish }",
        "  - { from: promote, to: promote_again }\n  - { from: promote_again, to: publish }",
    );
    assert!(matches!(
        refuse(&duplicate),
        SpecError::DuplicatePromote { writer, .. } if writer == "write"
    ));

    let unpromoted = "version: 1\nname: x\nnodes:\n  w: {kind: agent, role: r, prompt: p, tools: writer, max_turns: 3}\n";
    assert!(matches!(
        refuse(unpromoted),
        SpecError::UnpromotedWriter { writer } if writer == "w"
    ));
}

#[test]
fn publish_rules_are_enforced() {
    let not_promote = implement_with("    from: promote\n", "    from: check\n");
    assert!(matches!(
        refuse(&not_promote),
        SpecError::PublishSource { node, .. } if node == "publish"
    ));

    let bypass = format!("{IMPLEMENT}  - {{ from: check, to: publish }}\n");
    assert!(matches!(
        refuse(&bypass),
        SpecError::PublishSource { reason, .. } if reason.contains("without passing")
    ));
}
