//! `mneme eval` end to end: seed the synthetic corpus into a data
//! directory, resolve the checked-in questions template against it, run
//! the binary with the stub embedder and read what it wrote.
//!
//! The stub embedder's vectors carry no meaning, so these runs gate on
//! keyword mode; what is under test is the command (grid, metrics,
//! output files, exit status, the no-write check), not ranking quality.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

use common::corpus;

const BINARY: &str = env!("CARGO_BIN_EXE_mneme");
const TEMPLATE: &str = include_str!("fixtures/eval/questions.jsonl");

/// A seeded data directory and its resolved questions file.
async fn seeded() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    let corpus = corpus::seed_data_dir(&data).await.expect("seed corpus");
    let questions = tmp.path().join("questions.jsonl");
    std::fs::write(&questions, corpus.resolve_questions(TEMPLATE).unwrap()).unwrap();
    (tmp, questions)
}

fn run_eval(tmp: &Path, questions: &Path, tag: &str, extra: &[&str]) -> Output {
    Command::new(BINARY)
        .arg("eval")
        .arg(questions)
        .arg("--data-dir")
        .arg(tmp.join("data"))
        .arg("--out-dir")
        .arg(tmp.join("out"))
        .args(["--tag", tag, "--gate-mode", "keyword"])
        .args(extra)
        .env("MNEME_EMBEDDER", "stub")
        .env("MNEME_LOG", "off")
        .output()
        .expect("run mneme eval")
}

fn metrics(tmp: &Path, tag: &str) -> Value {
    let path = tmp.join("out").join(format!("metrics-{tag}.json"));
    serde_json::from_str(&std::fs::read_to_string(path).expect("metrics file")).unwrap()
}

#[test]
fn every_key_in_the_questions_template_resolves() {
    let lines: Vec<&str> = TEMPLATE
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(lines.len(), 46);
    for line in &lines {
        let q: Value = serde_json::from_str(line).unwrap();
        assert!(q["id"].is_string() && q["question"].is_string(), "{line}");
        assert!(
            corpus::SCOPES.contains(&q["scope"].as_str().unwrap()),
            "{line}"
        );
    }
}

#[tokio::test]
async fn template_resolves_to_ids_and_rejects_unknown_keys() {
    let h = common::Harness::new();
    let c = h.seed().await;
    let resolved = c.resolve_questions(TEMPLATE).unwrap();
    assert_eq!(resolved.lines().count(), 46);
    assert!(!resolved.contains("fact:") && !resolved.contains("pair:"));
    let first: Value = serde_json::from_str(resolved.lines().next().unwrap()).unwrap();
    assert_eq!(first["gold"][0], c.resolve("fact:global:41").unwrap());
    assert_eq!(
        c.resolve("pair:3:stale").unwrap(),
        c.pairs[3].stale.to_string()
    );
    assert_eq!(c.resolve("warm:10").unwrap(), c.warm[10].id.to_string());
    for bad in [
        "fact:nowhere:1",
        "fact:ops:50",
        "pair:10:current",
        "pair:1:old",
        "event:20",
        "x",
    ] {
        assert!(c.resolve(bad).is_none(), "{bad}");
    }
    let err = c
        .resolve_questions("{\"id\":\"a\",\"question\":\"?\",\"gold\":[\"fact:ops:99\"]}")
        .unwrap_err();
    assert!(
        err.contains("line 1") && err.contains("fact:ops:99"),
        "{err}"
    );
}

#[tokio::test]
async fn eval_measures_the_seeded_corpus_and_writes_nothing() {
    let (tmp, questions) = seeded().await;
    // The thresholds the nightly workflow uses.
    let out = run_eval(
        tmp.path(),
        &questions,
        "ok",
        &[
            "--min-l4-hit5",
            "0.95",
            "--min-context-gold",
            "0.80",
            "--max-context-stale-over-current",
            "0",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let m = metrics(tmp.path(), "ok");
    assert_eq!(m["tag"], "ok");
    assert_eq!(m["questions"]["total"], 46);
    assert_eq!(m["questions"]["with_l4_gold"], 34);
    assert_eq!(m["questions"]["l3_only"], 12);
    assert_eq!(m["questions"]["with_stale"], 10);

    // The grid: 3 modes x scope off/on x the layer sets the server has.
    let layer_sets = if m["server"]["recall_layers_supported"] == true {
        2
    } else {
        1
    };
    let grid = m["grid"].as_array().unwrap();
    assert_eq!(grid.len(), 3 * 2 * layer_sets);
    for cell in grid {
        assert_eq!(cell["errors"], serde_json::json!([]), "{cell}");
        assert_eq!(cell["l4"]["n"], 34);
        assert_eq!(cell["all"]["n"], 46);
        assert!(
            cell["latency_ms"]["p95"].as_f64().unwrap()
                >= cell["latency_ms"]["p50"].as_f64().unwrap()
        );
        if cell["scoped"] == true {
            assert_eq!(
                cell["off_scope_at_5"], 0.0,
                "a scope filter must hold: {cell}"
            );
        }
    }

    // Keyword mode finds every exact-token and fact question.
    let h = &m["headline"];
    assert_eq!(h["mode"], "keyword");
    assert!(h["l4_hit_at_5"].as_f64().unwrap() >= 0.95, "{h}");
    let keyword = grid
        .iter()
        .find(|c| c["mode"] == "keyword" && c["scoped"] == false && c["layers"] == "semantic")
        .unwrap();
    assert_eq!(keyword["by_type"]["exact"]["hit_at_1"], 1.0);
    assert_eq!(keyword["by_type"]["fact"]["hit_at_1"], 1.0);
    assert_eq!(keyword["stale_over_current"]["n"], 10);

    // Context replays ran for every question, within budget.
    let c = &m["context"];
    assert_eq!(c["n"], 46);
    assert_eq!(c["gold_n"], 46);
    assert_eq!(c["over_budget"], serde_json::json!([]));
    assert!(c["gold_rate"].as_f64().unwrap() >= 0.80, "{c}");
    assert_eq!(c["stale_over_current"]["count"], 0, "{c}");
    for layer in ["procedural", "working", "episodic", "semantic"] {
        let share = c["layer_shares"][layer].as_f64().unwrap();
        assert!((0.0..=1.0).contains(&share), "{layer}: {share}");
    }

    // Nothing was written.
    let w = &m["no_write_check"];
    assert_eq!(w["ok"], true, "{w}");
    assert_eq!(w["before"], w["after"]);
    assert_eq!(w["after"]["semantic"], 320);
    assert_eq!(w["after"]["procedural"], 6);
    assert_eq!(w["after"]["turns_total"], 0);

    let summary = std::fs::read_to_string(tmp.path().join("out").join("summary-ok.md")).unwrap();
    for needle in [
        "# mneme eval: ok",
        "## Headline (mode: keyword, unscoped)",
        "## Recall grid",
        "## Auto-context replay",
        "## No-write check\n\nPassed",
    ] {
        assert!(
            summary.contains(needle),
            "summary lacks `{needle}`:\n{summary}"
        );
    }
    assert!(
        stdout.contains("## Recall grid"),
        "the summary is printed too"
    );
}

#[tokio::test]
async fn eval_exits_nonzero_when_a_threshold_is_missed() {
    let (tmp, questions) = seeded().await;
    let out = run_eval(
        tmp.path(),
        &questions,
        "gate",
        &["--no-context", "--min-all-hit5", "0.999"],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("all hit@5 is"), "{stderr}");
    assert!(stderr.contains("below the required 0.999"), "{stderr}");
    // The output is written all the same: a failed gate needs its numbers.
    let m = metrics(tmp.path(), "gate");
    assert!(m["context"].is_null(), "--no-context skips the replays");
    assert_eq!(m["no_write_check"]["ok"], true);
}

#[test]
fn eval_rejects_a_bad_questions_file_before_starting_a_server() {
    let tmp = TempDir::new().unwrap();
    let questions = tmp.path().join("q.jsonl");
    std::fs::write(&questions, "{\"id\":\"a\",\"question\":\"x\"}\nnot json\n").unwrap();
    let out = run_eval(tmp.path(), &questions, "bad", &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("questions line 2"), "{stderr}");
    assert!(!tmp.path().join("data").exists(), "no data dir was created");
}
