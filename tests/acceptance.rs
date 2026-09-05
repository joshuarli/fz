use std::process::Command;

#[test]
fn fzy_acceptance_contract() {
    let status = Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/acceptance.py"))
        .arg(env!("CARGO_BIN_EXE_fz"))
        .status()
        .expect("python3 must be available to run the stdlib acceptance harness");
    assert!(status.success(), "acceptance harness failed: {status}");
}
