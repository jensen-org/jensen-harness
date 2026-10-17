//! Fixture outputs from real tools. Each asserts both what must survive and a
//! minimum saving, so a filter that quietly stops filtering fails the build.

use jensen_harness_core::compact::generic::{Limits, collapse_repeats, generic};
use jensen_harness_core::compact::render::{self, ReadArgs};
use jensen_harness_core::compact::shell::{filter, parse_command};
use jensen_harness_core::compact::store::RawStore;
use jensen_harness_core::compact::{CompactError, Compacted, tokens};
use jensen_harness_core::tools::edit::EditOutcome;
use jensen_harness_core::tools::grep::Hit;

fn assert_saves(raw: &str, sent: &str, at_least_percent: usize) {
    let (raw_tokens, sent_tokens) = (tokens(raw), tokens(sent));
    let saved = raw_tokens.saturating_sub(sent_tokens) * 100 / raw_tokens.max(1);
    assert!(
        saved >= at_least_percent,
        "saved {saved}% ({raw_tokens} -> {sent_tokens}), wanted {at_least_percent}%\n{sent}"
    );
}

fn passing_tests(count: usize) -> String {
    (0..count)
        .map(|i| format!("test tests::case_{i} ... ok"))
        .collect::<Vec<_>>()
        .join("\n")
}

const CARGO_FAILING_TAIL: &str = "
failures:

---- tests::broken stdout ----
thread 'tests::broken' panicked at tests/a.rs:9:5:
assertion `left == right` failed
  left: 1
 right: 2

failures:
    tests::broken

test result: FAILED. 40 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.31s

error: test failed, to rerun pass `--test a`";

#[test]
fn a_failing_cargo_test_keeps_the_failure_and_drops_the_passes() {
    let raw = format!(
        "   Compiling demo v0.1.0 (/work/demo)\n    Finished `test` profile [unoptimized] target(s) in 1.20s\n     Running tests/a.rs (target/debug/deps/a-1)\n\nrunning 41 tests\n{}\ntest tests::broken ... FAILED\n{CARGO_FAILING_TAIL}",
        passing_tests(40)
    );
    let out = filter("cargo test", &raw, Limits::default());
    assert!(out.text.contains("---- tests::broken stdout ----"));
    assert!(out.text.contains("left: 1"));
    assert!(
        out.text
            .contains("test result: FAILED. 40 passed; 1 failed")
    );
    assert!(!out.text.contains("case_7 ... ok"));
    assert!(out.lossy);
    assert_saves(&raw, &out.text, 60);
}

#[test]
fn an_all_green_cargo_test_is_only_its_result_line() {
    let raw = format!(
        "   Compiling demo v0.1.0\n     Running unittests (target/debug/deps/demo-1)\n\nrunning 60 tests\n{}\n\ntest result: ok. 60 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s\n",
        passing_tests(60)
    );
    let out = filter("cargo test --workspace", &raw, Limits::default());
    assert_eq!(
        out.text,
        "test result: ok. 60 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s"
    );
    assert_saves(&raw, &out.text, 90);
}

#[test]
fn a_cargo_build_keeps_diagnostics_and_drops_progress() {
    let raw = "   Compiling a v0.1.0\n   Compiling b v0.1.0\nwarning: unused variable: `x`\n --> src/lib.rs:3:9\n  |\n3 |     let x = 1;\n  |         ^ help: prefix it with an underscore\n\nwarning: `b` (lib) generated 1 warning\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.20s\n";
    let out = filter("cargo build", raw, Limits::default());
    assert!(out.text.contains("unused variable"));
    assert!(out.text.contains("src/lib.rs:3:9"));
    assert!(out.text.contains("Finished"));
    assert!(!out.text.contains("Compiling"));
}

#[test]
fn a_clean_cargo_check_is_one_word() {
    let raw = "    Checking a v0.1.0\n    Checking b v0.1.0\n    Finished `dev` profile in 1s\n";
    let out = filter("cargo check", raw, Limits::default());
    assert_eq!(out.text, "Finished `dev` profile in 1s");
}

#[test]
fn git_status_becomes_branch_and_changes() {
    let raw = "On branch feat/x\nYour branch is up to date with 'origin/feat/x'.\n\nChanges to be committed:\n  (use \"git restore --staged <file>...\" to unstage)\n\tmodified:   src/a.rs\n\tnew file:   src/b.rs\n\nChanges not staged for commit:\n  (use \"git add <file>...\" to update what will be committed)\n  (use \"git restore <file>...\" to discard changes in working directory)\n\tmodified:   README.md\n\nUntracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n\ttmp.txt\n";
    let out = filter("git status", raw, Limits::default());
    assert_eq!(
        out.text,
        "branch feat/x\nYour branch is up to date with 'origin/feat/x'.\nstaged M src/a.rs\nstaged A src/b.rs\nunstaged M README.md\nuntracked ? tmp.txt"
    );
    assert_saves(raw, &out.text, 40);
}

#[test]
fn git_log_is_a_short_hash_and_a_subject_per_commit() {
    let raw = "commit 5366566abcdef0123456789\nAuthor: A <a@x>\nDate:   Mon Oct 12 2026\n\n    docs: say the thing\n\ncommit 7832401abcdef0123456789\nAuthor: A <a@x>\nDate:   Mon Oct 11 2026\n\n    chore: name an owner\n\n    Longer body that must not be sent.\n";
    let out = filter("git log", raw, Limits::default());
    assert_eq!(
        out.text,
        "5366566 docs: say the thing\n7832401 chore: name an owner"
    );
}

#[test]
fn git_diff_drops_context_and_index_lines() {
    let raw = "diff --git a/src/a.rs b/src/a.rs\nindex 111..222 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,7 +1,7 @@ fn main\n context one\n context two\n-old line\n+new line\n context three\n context four\n";
    let out = filter("git diff", raw, Limits::default());
    assert_eq!(
        out.text,
        "## src/a.rs\n@@ -1,7 +1,7 @@ fn main\n-old line\n+new line"
    );
    assert!(out.lossy);
}

#[test]
fn pytest_keeps_failures_and_the_summary() {
    let raw = "============================= test session starts =============================\ncollected 3 items\n\ntests/test_a.py ..F                                                      [100%]\n\n=================================== FAILURES ===================================\n_________________________________ test_broken __________________________________\n\n    def test_broken():\n>       assert 1 == 2\nE       assert 1 == 2\n\ntests/test_a.py:7: AssertionError\n=========================== short test summary info ============================\nFAILED tests/test_a.py::test_broken - assert 1 == 2\n========================= 1 failed, 2 passed in 0.12s ==========================\n";
    let out = filter("pytest -q", raw, Limits::default());
    assert!(out.text.contains("assert 1 == 2"));
    assert!(out.text.contains("FAILED tests/test_a.py::test_broken"));
    assert!(out.text.contains("1 failed, 2 passed"));
    assert!(!out.text.contains("collected 3 items"));
}

#[test]
fn go_test_drops_run_and_pass_lines_and_counts_ok_packages() {
    let raw = "=== RUN   TestA\n--- PASS: TestA (0.00s)\n=== RUN   TestB\n    b_test.go:12: got 1 want 2\n--- FAIL: TestB (0.00s)\nFAIL\nFAIL\tdemo/b\t0.004s\nok  \tdemo/a\t0.002s\nok  \tdemo/c\t0.003s\n";
    let out = filter("go test ./...", raw, Limits::default());
    assert!(out.text.contains("b_test.go:12: got 1 want 2"));
    assert!(out.text.contains("--- FAIL: TestB"));
    assert!(out.text.contains("ok: 2 packages"));
    assert!(!out.text.contains("=== RUN"));
}

#[test]
fn js_test_drops_passing_lines() {
    let raw = "  ✓ adds (2 ms)\n  ✓ subtracts\n  ✕ divides (3 ms)\n\n  ● divides\n\n    expect(received).toBe(expected)\n\nTests: 1 failed, 2 passed, 3 total\n";
    let out = filter("npm test", raw, Limits::default());
    assert!(out.text.contains("✕ divides"));
    assert!(out.text.contains("Tests: 1 failed"));
    assert!(!out.text.contains("adds"));
}

#[test]
fn a_compound_command_gets_only_the_generic_filter() {
    let raw = format!("{}\n", passing_tests(5));
    for command in [
        "cargo test | head",
        "cargo test && echo done",
        "cargo test > out.txt",
        "echo $(cargo test)",
    ] {
        assert_eq!(parse_command(command), None, "{command}");
        assert!(
            filter(command, &raw, Limits::default())
                .text
                .contains("case_3 ... ok")
        );
    }
}

#[test]
fn the_command_is_parsed_through_env_prefixes_toolchains_and_git_options() {
    let parsed = parse_command("RUST_LOG=debug cargo +nightly test -p demo").unwrap();
    assert_eq!(
        (parsed.program.as_str(), parsed.subcommand.as_deref()),
        ("cargo", Some("test"))
    );
    let parsed = parse_command("git -C ../other status -s").unwrap();
    assert_eq!(
        (parsed.program.as_str(), parsed.subcommand.as_deref()),
        ("git", Some("status"))
    );
    let parsed = parse_command("/usr/bin/pytest -x").unwrap();
    assert_eq!(parsed.program, "pytest");
}

#[test]
fn generic_strips_ansi_collapses_repeats_and_keeps_head_and_tail_of_a_long_output() {
    let colored = "\u{1b}[31merror\u{1b}[0m: boom";
    assert_eq!(generic(colored, Limits::default()).text, "error: boom");

    let lines: Vec<String> = ["x"; 5].iter().map(|s| (*s).to_string()).collect();
    assert_eq!(collapse_repeats(&lines), vec!["x (x5)".to_string()]);

    let long: String = (0..500).map(|i| format!("line {i}\n")).collect();
    let out = generic(&long, Limits::default());
    assert!(out.lossy);
    assert!(out.text.starts_with("line 0\n"));
    assert!(out.text.contains("[... 400 lines cut ...]"));
    assert!(out.text.ends_with("line 499"));
    assert_saves(&long, &out.text, 70);
}

fn big_rust_file() -> String {
    let mut source = String::from("pub struct Widget {\n    pub size: u32,\n}\n\nimpl Widget {\n");
    for i in 0..40 {
        source.push_str(&format!(
            "    pub fn method_{i}(&self, input: &str) -> Result<String, String> {{\n"
        ));
        for step in 0..10 {
            source.push_str(&format!(
                "        let step_{step} = input.repeat({step}).replace(\"a\", \"b\");\n"
            ));
        }
        source.push_str("        Ok(step_9)\n    }\n\n");
    }
    source.push_str("}\n\npub fn top_level(count: usize) -> usize {\n    count + 1\n}\n");
    source
}

#[test]
fn a_large_source_file_reads_as_an_outline_and_a_symbol_read_returns_one_function() {
    let source = big_rust_file();
    let outline = render::read("src/widget.rs", &source, &ReadArgs::default()).unwrap();
    assert!(outline.text.contains("outline only"));
    assert!(outline.text.contains("pub struct Widget"));
    assert!(
        outline
            .text
            .contains("pub fn method_7(&self, input: &str) -> Result<String, String>")
    );
    assert!(!outline.text.contains("input.repeat"));
    assert_saves(&source, &outline.text, 70);

    let symbol = render::read(
        "src/widget.rs",
        &source,
        &ReadArgs {
            symbol: Some("top_level".to_string()),
            ..ReadArgs::default()
        },
    )
    .unwrap();
    assert!(symbol.text.contains("count + 1"));
    assert!(!symbol.text.contains("method_3"));
    assert!(symbol.text.starts_with("src/widget.rs:"));
}

#[test]
fn a_small_file_is_sent_whole_and_a_range_is_numbered() {
    let source = "alpha\nbeta\ngamma\ndelta\n";
    let whole = render::read("notes.txt", source, &ReadArgs::default()).unwrap();
    assert_eq!(whole.text, source);

    let range = render::read(
        "notes.txt",
        source,
        &ReadArgs {
            start_line: Some(2),
            end_line: Some(3),
            ..ReadArgs::default()
        },
    )
    .unwrap();
    assert_eq!(range.text, "2\tbeta\n3\tgamma");
}

#[test]
fn a_missing_symbol_says_so_and_lists_what_exists() {
    let source = big_rust_file();
    let out = render::read(
        "src/widget.rs",
        &source,
        &ReadArgs {
            symbol: Some("nope".to_string()),
            ..ReadArgs::default()
        },
    )
    .unwrap();
    assert!(out.text.starts_with("no declaration named nope"));
    assert!(out.text.contains("top_level"));
}

#[test]
fn python_typescript_and_go_outlines_list_their_declarations() {
    use jensen_harness_core::compact::outline::outline;
    let python =
        "class Repo:\n    def save(self, item):\n        return item\n\ndef main():\n    pass\n";
    let text = outline("a.py", python).unwrap().unwrap();
    assert!(
        text.contains("class Repo")
            && text.contains("def save(self, item)")
            && text.contains("def main()")
    );

    let ts = "export interface User { id: number }\nexport class Store {\n  get(id: number): User { return { id }; }\n}\nexport function make(): Store { return new Store(); }\n";
    let text = outline("a.ts", ts).unwrap().unwrap();
    assert!(
        text.contains("interface User")
            && text.contains("class Store")
            && text.contains("function make()")
    );

    let go = "package demo\n\ntype Repo struct{}\n\nfunc (r Repo) Save() error { return nil }\n\nfunc Main() {}\n";
    let text = outline("a.go", go).unwrap().unwrap();
    assert!(
        text.contains("type Repo")
            && text.contains("func (r Repo) Save() error")
            && text.contains("func Main()")
    );

    assert!(outline("a.txt", "plain").unwrap().is_none());
}

#[test]
fn grep_results_group_by_file_cap_each_file_and_report_what_is_left() {
    let hits: Vec<Hit> = (0..200)
        .map(|i| Hit {
            path: format!("src/file_{}.rs", i % 4),
            line_number: i + 1,
            line: format!("    let needle_{i} = compute_something_expensive(argument_{i});"),
        })
        .collect();
    let out = render::grep(&hits);
    assert!(out.text.contains("src/file_0.rs (50)"));
    assert!(out.text.contains("+42 more in this file"));
    assert!(
        out.text
            .contains("matches shown across 4 files; narrow the pattern or the path")
    );
    assert_saves(
        &hits
            .iter()
            .map(|h| format!("{}:{}:{}", h.path, h.line_number, h.line))
            .collect::<Vec<_>>()
            .join("\n"),
        &out.text,
        70,
    );
    assert_eq!(render::grep(&[]).text, "no matches");
}

#[test]
fn a_long_glob_becomes_directories_with_counts() {
    let paths: Vec<String> = (0..120)
        .map(|i| format!("{}/file_{i}.rs", if i % 2 == 0 { "src" } else { "tests" }))
        .collect();
    let out = render::glob(&paths);
    assert!(out.text.starts_with("120 files in 2 directories"));
    assert!(out.text.contains("src/ (60):"));
    assert!(out.text.contains("+55 more"));
    assert_saves(&paths.join("\n"), &out.text, 60);

    let short = vec!["a.rs".to_string(), "b.rs".to_string()];
    assert_eq!(render::glob(&short).text, "a.rs\nb.rs");
}

#[test]
fn an_edit_answers_in_one_line() {
    let out = render::edit(
        "src/a.rs",
        &EditOutcome {
            replacements: 1,
            first_line: 12,
        },
    );
    assert_eq!(out.text, "edited src/a.rs: 1 replacement, first at line 12");
}

#[test]
fn a_filtered_shell_result_is_teed_and_expand_returns_the_exact_raw_lines() {
    let dir = tempfile::tempdir().unwrap();
    let store = RawStore::new(dir.path());
    let stdout = format!(
        "running 41 tests\n{}\ntest tests::broken ... FAILED\n{CARGO_FAILING_TAIL}\n",
        passing_tests(40)
    );

    let handle = store.next_handle();

    let out: Compacted = render::shell("cargo test", 101, &stdout, "", &store).unwrap();

    assert!(out.text.starts_with("exit 101\n"));
    assert!(
        out.text
            .ends_with(&format!("[filtered; full output: expand {handle}]"))
    );
    assert!(out.sent_tokens < out.raw_tokens / 2);

    let raw = render::expand(&store, &handle, 0, 400).unwrap();
    let expected = format!("stdout:\n{stdout}\nstderr:\n");
    assert_eq!(raw.text, expected.lines().collect::<Vec<_>>().join("\n"));
    let window = render::expand(&store, &handle, 2, 3).unwrap();
    assert!(
        window
            .text
            .starts_with("test tests::case_0 ... ok\ntest tests::case_1 ... ok")
    );
    assert!(window.text.contains("call expand again with offset 5"));
}

#[test]
fn an_unfiltered_shell_result_is_not_teed() {
    let dir = tempfile::tempdir().unwrap();
    let store = RawStore::new(dir.path());
    let out = render::shell("echo hi", 0, "hi\n", "", &store).unwrap();
    assert_eq!(out.text, "hi");
    assert!(render::expand(&store, &store.next_handle(), 0, 10).is_err());
}

#[test]
fn only_handles_this_store_issued_can_be_expanded() {
    let dir = tempfile::tempdir().unwrap();
    let store = RawStore::new(dir.path());
    for handle in [
        "../secret",
        "r1/../../x",
        "r",
        "x1",
        "r1.txt",
        "r1",
        "r-x-1",
        "r--1",
        "",
    ] {
        assert!(
            matches!(
                store.slice(handle, 0, 5),
                Err(CompactError::UnknownHandle(_))
            ),
            "{handle:?}"
        );
    }
}

#[test]
fn an_empty_result_and_a_failing_silent_command_are_still_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let store = RawStore::new(dir.path());
    assert_eq!(
        render::shell("true", 0, "", "", &store).unwrap().text,
        "(no output)"
    );
    assert_eq!(
        render::shell("false", 1, "", "", &store).unwrap().text,
        "exit 1\n(no output)"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_planted_at_the_next_handle_is_refused_and_the_target_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("config");
    std::fs::write(&target, "original").unwrap();
    let store = RawStore::new(dir.path());
    let next = store.next_handle();
    std::os::unix::fs::symlink(&target, dir.path().join(format!("{next}.txt"))).unwrap();

    let error = store.tee("attacker controlled output").unwrap_err();

    assert!(matches!(error, CompactError::Store(_)), "{error:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
}

#[test]
fn a_file_planted_under_a_handle_that_was_never_issued_cannot_be_expanded() {
    let dir = tempfile::tempdir().unwrap();
    let store = RawStore::new(dir.path());
    let future = store.next_handle().replace("-1", "-7");
    std::fs::write(dir.path().join(format!("{future}.txt")), "planted").unwrap();
    assert!(matches!(
        store.slice(&future, 0, 10),
        Err(CompactError::UnknownHandle(_))
    ));

    let issued = store.tee("real").unwrap();
    assert_eq!(store.slice(&issued, 0, 10).unwrap(), "real");
    let after = issued.replace("-1", "-2");
    assert!(matches!(
        store.slice(&after, 0, 10),
        Err(CompactError::UnknownHandle(_))
    ));
    let zero = issued.replace("-1", "-0");
    assert!(matches!(
        store.slice(&zero, 0, 10),
        Err(CompactError::UnknownHandle(_))
    ));
}

#[test]
fn a_handle_from_one_store_is_refused_by_another_even_after_it_has_issued_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let before_restart = RawStore::new(dir.path());
    let old = before_restart
        .tee("the output the model was told about")
        .unwrap();

    let after_restart = RawStore::new(dir.path());
    let new = after_restart.tee("a different command's output").unwrap();

    assert_ne!(old, new, "handles do not repeat across stores");
    for count in 0..3 {
        assert!(
            matches!(
                after_restart.slice(&old, 0, 10),
                Err(CompactError::UnknownHandle(_))
            ),
            "attempt {count}"
        );
        after_restart.tee("more").unwrap();
    }
    assert_eq!(
        before_restart.slice(&old, 0, 10).unwrap(),
        "the output the model was told about"
    );
}

#[cfg(unix)]
#[test]
fn the_store_directory_is_private_to_its_owner() {
    use std::os::unix::fs::PermissionsExt;
    let parent = tempfile::tempdir().unwrap();
    let store = RawStore::new(parent.path().join("raw"));
    store.tee("x").unwrap();
    let mode = std::fs::metadata(parent.path().join("raw"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[test]
fn a_pattern_that_matches_everywhere_stops_collecting_and_says_there_is_more() {
    use jensen_harness_core::tools::{Worktree, grep};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("big.txt"),
        "needle\n".repeat(grep::MAX_HITS + 500),
    )
    .unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();

    let hits = grep::run(&worktree, ".", "needle").unwrap();

    assert_eq!(hits.len(), grep::MAX_HITS);
    let out = render::grep(&hits);
    assert!(
        out.text.contains("2000+ matches shown") || out.text.contains("of 2000+"),
        "{}",
        out.text
    );
}
