//! A cross-platform stand-in for `gh`/`glab` (and `cargo`/`git-stk`/`pwsh`)
//! in the test suite, replacing the Unix-only `sh` fakes so the provider
//! suites can run on Windows too.
//!
//! Behavior is driven by a JSON spec (path in `STK_FAKE_SPEC`): an ordered
//! list of rules matched against the space-joined arguments, first match
//! wins - mirroring an `sh` `case "$*"`. A rule prints stdout/stderr, exits
//! with a code, and may record the arguments to a file (truncating or
//! appending, for invocation assertions). A rule can also require a marker
//! file to exist (`if_file`), which models a provider whose answers change
//! after a side effect like a merge, and can save what it read on stdin
//! (`stdin`). An optional spec-level `log` records every invocation. Built only under the `test-fakes` feature, so it never
//! ships.

use std::io::Write;
use std::path::Path;

use serde_json::Value;

fn append_line(file: &str, line: &str, append: bool) {
    let mut handle = std::fs::OpenOptions::new()
        .create(true)
        .append(append)
        .truncate(!append)
        .write(true)
        .open(file)
        .expect("open record file");
    writeln!(handle, "{line}").expect("write record file");
}

fn main() -> std::process::ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>().join(" ");

    let spec_path = match std::env::var("STK_FAKE_SPEC") {
        Ok(path) => path,
        Err(_) => {
            eprintln!("fake-provider: STK_FAKE_SPEC is not set");
            return std::process::ExitCode::FAILURE;
        }
    };
    let spec = std::fs::read_to_string(&spec_path).expect("read fake spec");
    let spec: Value = serde_json::from_str(&spec).expect("parse fake spec");

    // Spec-level log of every invocation, in order (asserts call sequencing).
    if let Some(log) = spec["log"].as_str() {
        append_line(log, &args, true);
    }

    let rules = spec["rules"].as_array().expect("spec rules array");
    for rule in rules {
        let needle = rule["contains"].as_str().unwrap_or("");
        if !args.contains(needle) {
            continue;
        }
        // A marker file gates the rule: the provider answers differently once
        // a side effect (e.g. a recorded merge) has created it.
        if let Some(marker) = rule["if_file"].as_str()
            && !Path::new(marker).exists()
        {
            continue;
        }

        if let Some(file) = rule["record"].as_str() {
            append_line(file, &args, rule["append"].as_bool().unwrap_or(false));
        }
        if let Some(file) = rule["stdin"].as_str() {
            let mut input = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).expect("read stdin");
            append_line(file, &input, false);
        }
        if let Some(out) = rule["stdout"].as_str()
            && !out.is_empty()
        {
            print!("{out}");
            if !out.ends_with('\n') {
                println!();
            }
        }
        if let Some(err) = rule["stderr"].as_str()
            && !err.is_empty()
        {
            eprintln!("{err}");
        }
        let code = rule["exit"].as_i64().unwrap_or(0) as u8;
        return std::process::ExitCode::from(code);
    }

    // No rule matched: surface it loudly so a missing case is obvious rather
    // than silently returning nothing.
    eprintln!("fake-provider: no rule matched args: {args}");
    std::process::ExitCode::FAILURE
}
