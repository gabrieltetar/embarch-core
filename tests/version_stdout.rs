//! Pins `embarch-core --version`'s stdout to exactly one line: `embarch-core
//! <version>`, nothing else. `embarch-umbrella`'s doctor check 1 (and any
//! other future caller) reads `output.stdout` alone and trims it — a
//! `--version` invocation that puts anything else there, before or after,
//! breaks that read (core/tasks/015). This does not force the fallback
//! logging arm to fail (that needs an unwritable machine-wide log
//! directory, which won't reproduce in CI or on a dev box run as a normal
//! user — see `src/main.rs`'s `init_tracing_tests` for a test that pins
//! *that* arm's writer choice directly instead); it guards the ordinary,
//! common-case path every invocation takes.

use std::process::Command;

#[test]
fn version_stdout_is_exactly_one_line() {
    let exe = env!("CARGO_BIN_EXE_embarch-core");
    let output = Command::new(exe)
        .arg("--version")
        .output()
        .expect("failed to run embarch-core --version");

    let stdout = String::from_utf8(output.stdout).expect("stdout was not valid UTF-8");
    let lines: Vec<&str> = stdout.lines().collect();

    assert_eq!(
        lines.len(),
        1,
        "expected exactly one line of stdout from --version, got: {stdout:?}"
    );
    assert!(
        lines[0].starts_with("embarch-core "),
        "expected the version line to start with \"embarch-core \", got: {:?}",
        lines[0]
    );
    assert!(
        !stdout.contains('\u{1b}'),
        "expected no ANSI escapes on stdout, got: {stdout:?}"
    );
}
