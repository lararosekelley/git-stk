//! Shared integration-test harness.
#![allow(dead_code)]

use std::{env, fs, path::Path, process::Command};

use tempfile::TempDir;

pub struct TestRepo {
    dir: TempDir,
}

impl TestRepo {
    pub fn new() -> Self {
        let repo = Self {
            dir: tempfile::tempdir().expect("create temp repo"),
        };
        repo.git(["init", "--initial-branch", "main"]);
        repo.git(["config", "user.email", "test@example.com"]);
        repo.git(["config", "user.name", "Test User"]);
        repo.write("README.md", "# test repo\n");
        repo.git(["add", "README.md"]);
        repo.git(["commit", "-m", "initial commit"]);
        repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn write(&self, path: &str, contents: &str) {
        fs::write(self.path().join(path), contents).expect("write test file");
    }

    /// Isolate a command from the developer's global and system git config so
    /// the suite stays hermetic (e.g. a global stk.pushOnSubmit=true must not
    /// change test behavior).
    pub fn isolate_git_config(command: &mut Command) {
        command.env("GIT_CONFIG_GLOBAL", nul_device());
        command.env("GIT_CONFIG_NOSYSTEM", "1");
    }

    pub fn git<const N: usize>(&self, args: [&str; N]) -> String {
        let mut command = Command::new("git");
        Self::isolate_git_config(&mut command);
        let output = command
            .args(args)
            .current_dir(self.path())
            .output()
            .expect("run git command");

        assert!(
            output.status.success(),
            "git failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    pub fn git_status<const N: usize>(&self, args: [&str; N]) -> std::process::Output {
        let mut command = Command::new("git");
        Self::isolate_git_config(&mut command);
        command
            .args(args)
            .current_dir(self.path())
            .output()
            .expect("run git command")
    }

    pub fn stack_output<const N: usize>(&self, args: [&str; N]) -> std::process::Output {
        let mut command = self.stack();
        command.args(args).output().expect("run git-stk command")
    }

    pub fn supports_update_refs(&self) -> bool {
        let output = self.git_status(["rebase", "-h"]);
        let help = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // Match the name: git may render it as --[no-]update-refs.
        help.contains("update-refs")
    }

    pub fn commit_file(&self, path: &str, contents: &str, message: &str) {
        self.write(path, contents);
        self.git(["add", path]);
        self.git(["commit", "-m", message]);
    }

    pub fn stack(&self) -> assert_cmd::Command {
        self.stack_in(self.path())
    }

    /// Like [`stack`](Self::stack) but run from `dir` - e.g. a linked worktree
    /// of this repo - instead of the main worktree root.
    pub fn stack_in(&self, dir: &Path) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::cargo_bin("git-stk").expect("git-stk binary");
        command.current_dir(dir);
        command.env("GIT_EDITOR", "true");
        command.env("GIT_CONFIG_GLOBAL", nul_device());
        command.env("GIT_CONFIG_NOSYSTEM", "1");
        // Hermetic color: ambient terminal settings must not restyle output.
        command.env_remove("CLICOLOR");
        command.env_remove("CLICOLOR_FORCE");
        command.env_remove("NO_COLOR");
        command
    }
}

/// A cross-platform fake for the external commands git-stk shells out to -
/// `gh`/`glab` by default, or any others (`cargo`, `git-stk`, `pwsh`) named
/// via [`commands`](FakeProvider::commands). Ordered rules are matched
/// against the joined arguments (first match wins, like an `sh` `case
/// "$*"`), realized by the `git-stk-fake-provider` helper binary. Replaces
/// the Unix-only `fake_cli` shell scripts so suites can run on Windows too.
pub struct FakeProvider {
    rules: Vec<serde_json::Value>,
    commands: Vec<String>,
    log: Option<String>,
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            commands: vec!["gh".into(), "glab".into()],
            log: None,
        }
    }
}

impl FakeProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the fake under these command names instead of `gh`/`glab`
    /// (e.g. `cargo`, `git-stk`, `pwsh`). They all share one rule set.
    pub fn commands(mut self, names: &[&str]) -> Self {
        self.commands = names.iter().map(|n| n.to_string()).collect();
        self
    }

    /// Record every invocation's arguments to `file`, in order - for
    /// asserting the sequence of provider calls.
    pub fn log_all(mut self, file: &str) -> Self {
        self.log = Some(file.to_string());
        self
    }

    /// Respond with `stdout` when the joined args contain `needle`.
    pub fn on(mut self, needle: &str, stdout: &str) -> Self {
        self.rules
            .push(serde_json::json!({ "contains": needle, "stdout": stdout }));
        self
    }

    /// Like [`on`](Self::on), but only once `marker_file` exists. Pair it
    /// with a [`record`](Self::record) of the side effect that creates the
    /// marker to model a provider whose answer flips after a merge/retarget.
    pub fn on_after(mut self, needle: &str, marker_file: &str, stdout: &str) -> Self {
        self.rules.push(serde_json::json!({
            "contains": needle, "if_file": marker_file, "stdout": stdout,
        }));
        self
    }

    /// Respond with `stdout` and overwrite `record_file` with this
    /// invocation (last write wins, like `> file`).
    pub fn record(mut self, needle: &str, record_file: &str, stdout: &str) -> Self {
        self.rules.push(serde_json::json!({
            "contains": needle, "stdout": stdout, "record": record_file,
        }));
        self
    }

    /// Respond with `stdout` and append this invocation to `record_file`
    /// (every call kept, like `>> file`).
    pub fn record_append(mut self, needle: &str, record_file: &str, stdout: &str) -> Self {
        self.rules.push(serde_json::json!({
            "contains": needle, "stdout": stdout, "record": record_file, "append": true,
        }));
        self
    }

    /// Fail (exit 1) with `stderr` when the joined args contain `needle`.
    pub fn fail(mut self, needle: &str, stderr: &str) -> Self {
        self.rules
            .push(serde_json::json!({ "contains": needle, "stderr": stderr, "exit": 1 }));
        self
    }

    /// Fail (exit 1) with `stdout` as well as `stderr` - for a command like
    /// `gh pr checks` that prints its result table to stdout even when it
    /// exits non-zero on a failure.
    pub fn fail_with_stdout(mut self, needle: &str, stdout: &str, stderr: &str) -> Self {
        self.rules.push(serde_json::json!({
            "contains": needle, "stdout": stdout, "stderr": stderr, "exit": 1,
        }));
        self
    }

    /// The catch-all response (empty needle matches anything).
    pub fn fallback(mut self, stdout: &str) -> Self {
        self.rules
            .push(serde_json::json!({ "contains": "", "stdout": stdout }));
        self
    }

    /// A catch-all that fails (exit 1) - a guard asserting no unexpected call
    /// slips through.
    pub fn fallback_fail(mut self, stderr: &str) -> Self {
        self.rules
            .push(serde_json::json!({ "contains": "", "stderr": stderr, "exit": 1 }));
        self
    }

    /// Write the spec and drop copies of the fake binary (one per command
    /// name) on a PATH dir. Returns the env values the command needs.
    pub fn install(self, repo: &TestRepo) -> FakeProviderEnv {
        // Present only when built under `test-fakes` (always so via `just
        // test`); a `match` rather than `expect` keeps clippy happy while
        // still failing loudly if a bare `cargo test` reaches here.
        let bin = match option_env!("CARGO_BIN_EXE_git-stk-fake-provider") {
            Some(path) => path,
            None => panic!("build tests with the `test-fakes` feature (use `just test`)"),
        };

        // Live under .git so the fake binaries and spec never surface as
        // untracked files - some commands (e.g. `run`) refuse a dirty tree.
        let support = repo.path().join(".git").join("stk-fake");
        let bin_dir = support.join("bin");
        fs::create_dir_all(&bin_dir).expect("create fake bin dir");
        for name in &self.commands {
            let dest = bin_dir.join(format!("{name}{}", env::consts::EXE_SUFFIX));
            fs::copy(bin, &dest).expect("copy fake provider");
        }

        let spec_path = support.join("spec.json");
        let mut spec = serde_json::json!({ "rules": self.rules });
        if let Some(log) = self.log {
            spec["log"] = serde_json::Value::String(log);
        }
        fs::write(&spec_path, spec.to_string()).expect("write fake spec");

        let existing = env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![bin_dir];
        dirs.extend(env::split_paths(&existing));
        let path = env::join_paths(dirs).expect("join PATH");

        FakeProviderEnv {
            path: path.to_string_lossy().into_owned(),
            spec: spec_path.to_string_lossy().into_owned(),
        }
    }
}

/// The `PATH` and `STK_FAKE_SPEC` a faked command needs.
pub struct FakeProviderEnv {
    pub path: String,
    pub spec: String,
}

impl TestRepo {
    /// A `git stk` command wired to the given provider fake.
    pub fn stack_faked(&self, fake: &FakeProviderEnv) -> assert_cmd::Command {
        let mut command = self.stack();
        command
            .env("PATH", &fake.path)
            .env("STK_FAKE_SPEC", &fake.spec);
        command
    }
}

impl TestRepo {
    /// Create a bare repo, add it as origin, and push the given branches.
    pub fn add_bare_origin(&self, branches: &[&str]) -> TempDir {
        let bare = tempfile::tempdir().expect("create bare remote");
        Command::new("git")
            .args(["init", "--bare", "--initial-branch", "main"])
            .arg(bare.path())
            .output()
            .expect("init bare remote");

        self.git(["remote", "add", "origin", bare.path().to_str().unwrap()]);
        for branch in branches {
            self.git(["push", "-u", "origin", branch]);
        }
        bare
    }

    pub fn remote_sha(&self, bare: &TempDir, branch: &str) -> String {
        let output = Command::new("git")
            .args(["rev-parse", branch])
            .current_dir(bare.path())
            .output()
            .expect("rev-parse remote branch");
        assert!(output.status.success(), "remote branch {branch} missing");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }
}

impl TestRepo {
    /// Run the bash completion harness: source the registration script, set
    /// up COMP_WORDS for `git stk <words...><TAB>`, invoke the _git_stk shim,
    /// and return COMPREPLY entries.
    /// Unix-only: completion assertions run through a bash harness.
    #[cfg(unix)]
    pub fn complete_git_stk(&self, words: &[&str]) -> String {
        let output = self.stack_output(["completions", "bash"]).stdout;
        let script_path = self.path().join("completions.bash");
        fs::write(&script_path, output).expect("write completions script");

        let comp_words = words
            .iter()
            .map(|word| format!("\"{word}\""))
            .collect::<Vec<_>>()
            .join(" ");
        let harness = format!(
            r#"source "{}"
COMP_WORDS=(git stk {comp_words})
COMP_CWORD={}
_git_stk
printf '%s\n' "${{COMPREPLY[@]}}"
"#,
            script_path.display(),
            words.len() + 1,
        );

        let mut command = Command::new("bash");
        Self::isolate_git_config(&mut command);
        let result = command
            .args(["-c", &harness])
            .current_dir(self.path())
            .output()
            .expect("run bash completion harness");
        assert!(
            result.status.success(),
            "harness failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8_lossy(&result.stdout).into_owned()
    }
}

/// Git's "no config file" sink, per platform.
pub fn nul_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}
