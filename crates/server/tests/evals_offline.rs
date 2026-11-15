//! The offline half of the model evaluation in `evals/`: its own tests and the golden set's check
//! (shape, no golden input in a prompt or tool description, recorded verdicts against hand labels).
//! It runs here so that `cargo test --workspace` covers it. Needs `python3`; makes no network call.

use std::path::Path;
use std::process::Command;

fn python(args: &[&str]) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = Command::new("python3")
        .arg("-I")
        .args(args)
        .current_dir(&root)
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("python3 runs");
    assert!(
        out.status.success(),
        "python3 {args:?} failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_evaluators_pass_their_own_tests() {
    python(&["evals/test_evals.py"]);
}

#[test]
fn the_golden_set_is_well_formed_and_absent_from_every_prompt() {
    python(&["evals/run.py", "check"]);
}
