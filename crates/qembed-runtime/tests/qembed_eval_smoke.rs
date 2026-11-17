//! Smoke test for the `qembed-eval` binary target.

use std::process::Command;

#[test]
fn qembed_eval_binary_starts_successfully() {
    let output = Command::new(env!("CARGO_BIN_EXE_qembed-eval"))
        .output()
        .expect("qembed-eval should be built and executable by Cargo");

    assert!(
        output.status.success(),
        "qembed-eval exited unsuccessfully; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
