// These suites drive sh-script provider fakes, so they are Unix-only.
#![cfg(unix)]

use std::fs;
mod common;

use common::{FakeProvider, TestRepo};
use predicates::prelude::PredicateBooleanExt;

#[test]
fn setup_installs_man_page_and_wires_bashrc() {
    let repo = TestRepo::new();
    let home = repo.path().join("home");
    fs::create_dir_all(&home).expect("create fake home");

    // --yes wires completions non-interactively (the test harness has no TTY,
    // and setup now only prompts at a real terminal).
    repo.stack()
        .args(["setup", "--yes"])
        .env("HOME", &home)
        .env_remove("XDG_DATA_HOME")
        .env("SHELL", "/bin/bash")
        .assert()
        .success()
        .stdout(predicates::str::contains("installed man page"))
        .stdout(predicates::str::contains("added bash completion setup"));

    assert!(home.join(".local/share/man/man1/git-stk.1").exists());
    let rc = fs::read_to_string(home.join(".bashrc")).expect("read bashrc");
    assert!(rc.contains("command -v git-stk >/dev/null && source <(git stk completions bash)"));
}

#[test]
fn setup_wires_powershell_when_no_posix_shell() {
    let repo = TestRepo::new();
    let profile = repo.path().join("Documents/PowerShell/profile.ps1");
    // A fake PowerShell that reports its $PROFILE path (the real query the
    // setup runs), whatever it is invoked with. Its parent dir does not exist
    // yet - setup must create it.
    let profile_path = profile.display().to_string();
    let fake = FakeProvider::new()
        .commands(&["pwsh"])
        .fallback(&profile_path)
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["setup", "--yes"])
        .env_remove("SHELL") // no POSIX shell -> fall through to PowerShell
        .env_remove("XDG_DATA_HOME")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "added PowerShell completion setup",
        ));

    let rc = fs::read_to_string(&profile).expect("powershell profile written");
    assert!(rc.contains("git stk completions powershell | Out-String | Invoke-Expression"));
    assert!(rc.contains("Get-Command git-stk -ErrorAction SilentlyContinue"));
}

#[test]
fn setup_warns_and_skips_powershell_when_execution_policy_blocks_the_profile() {
    let repo = TestRepo::new();
    let profile = repo.path().join("Documents/PowerShell/profile.ps1");
    let profile_path = profile.display().to_string();

    // A fake PowerShell whose execution policy is Restricted: writing a profile
    // would only produce a "not digitally signed" error on every shell start.
    let fake = FakeProvider::new()
        .commands(&["pwsh"])
        .on("Get-ExecutionPolicy", "Restricted")
        .on("$PROFILE", &profile_path)
        .fallback(&profile_path)
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["setup", "--yes"])
        .env_remove("SHELL")
        .env_remove("XDG_DATA_HOME")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "execution policy (Restricted) blocks profile scripts",
        ))
        .stdout(predicates::str::contains(
            "Set-ExecutionPolicy -Scope CurrentUser RemoteSigned",
        ))
        // It must not claim to have wired anything up.
        .stdout(predicates::str::contains("added PowerShell completion setup").not());

    // No profile was written - the shell stays loadable.
    assert!(
        !profile.exists(),
        "must not create a profile that can't run"
    );
}

#[test]
fn setup_is_idempotent_for_completions() {
    let repo = TestRepo::new();
    let home = repo.path().join("home");
    fs::create_dir_all(&home).expect("create fake home");

    for _ in 0..2 {
        repo.stack()
            .args(["setup", "--yes"])
            .env("HOME", &home)
            .env("SHELL", "/bin/zsh")
            .assert()
            .success();
    }

    let rc = fs::read_to_string(home.join(".zshrc")).expect("read zshrc");
    assert_eq!(rc.matches("git stk completions zsh").count(), 1);
}

/// A fake HOME under the repo's tempdir.
fn fake_home(repo: &TestRepo) -> std::path::PathBuf {
    let home = repo.path().join("home");
    fs::create_dir_all(&home).expect("create fake home");
    home
}

#[test]
fn setup_wrapper_writes_the_stk_function_and_completion_alias() {
    let repo = TestRepo::new();
    let home = fake_home(&repo);

    repo.stack()
        .args(["setup", "--yes", "--wrapper"])
        .env("HOME", &home)
        .env("SHELL", "/bin/bash")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "added bash completion setup and the stk wrapper",
        ));

    let rc = fs::read_to_string(home.join(".bashrc")).expect("read bashrc");
    assert!(rc.contains("command -v git-stk >/dev/null && source <(git stk completions bash)"));
    assert!(rc.contains("stk() {"), "{rc}");
    assert!(rc.contains("up|down|top|bottom"), "{rc}");
    // The path is captured before the cd, so a failed navigation does not also
    // produce bash's own `cd: null directory`.
    assert!(
        rc.contains("dest=$(git stk \"$@\" --from-path) || return"),
        "{rc}"
    );
    assert!(rc.contains("complete -p git-stk"), "{rc}");
    assert!(rc.contains("# end git-stk setup"), "{rc}");
}

#[test]
fn setup_without_the_wrapper_advertises_it() {
    let repo = TestRepo::new();
    let home = fake_home(&repo);

    repo.stack()
        .args(["setup", "--yes"])
        .env("HOME", &home)
        .env("SHELL", "/bin/bash")
        .assert()
        .success()
        .stdout(predicates::str::contains("git stk setup --wrapper"));

    let rc = fs::read_to_string(home.join(".bashrc")).expect("read bashrc");
    assert!(!rc.contains("stk() {"), "wrapper must be opt-in: {rc}");
}

#[test]
fn setup_wrapper_merges_into_a_block_it_already_wrote() {
    let repo = TestRepo::new();
    let home = fake_home(&repo);

    // Completions first, wrapper later - the upgrade path for anyone who ran
    // setup before the wrapper existed.
    for args in [
        ["setup", "--yes"].as_slice(),
        ["setup", "--yes", "--wrapper"].as_slice(),
    ] {
        repo.stack()
            .args(args)
            .env("HOME", &home)
            .env("SHELL", "/bin/bash")
            .assert()
            .success();
    }

    let rc = fs::read_to_string(home.join(".bashrc")).expect("read bashrc");
    // Merged into the one block rather than appended as a second copy.
    assert_eq!(rc.matches("git stk completions bash").count(), 1, "{rc}");
    assert_eq!(rc.matches("# added by git-stk setup").count(), 1, "{rc}");
    assert_eq!(rc.matches("stk() {").count(), 1, "{rc}");
}

#[test]
fn setup_skips_the_wrapper_when_the_name_is_taken() {
    let repo = TestRepo::new();
    let home = fake_home(&repo);
    fs::write(home.join(".bashrc"), "alias stk=/usr/bin/something-else\n").expect("seed rc");

    repo.stack()
        .args(["setup", "--yes", "--wrapper"])
        .env("HOME", &home)
        .env("SHELL", "/bin/bash")
        .assert()
        .success()
        .stdout(predicates::str::contains("skipped the stk wrapper"));

    let rc = fs::read_to_string(home.join(".bashrc")).expect("read bashrc");
    assert!(rc.contains("alias stk=/usr/bin/something-else"), "{rc}");
    assert!(!rc.contains("stk() {"), "must not shadow their stk: {rc}");
}

#[test]
fn setup_declines_the_wrapper_for_fish() {
    let repo = TestRepo::new();
    let home = fake_home(&repo);

    repo.stack()
        .args(["setup", "--yes", "--wrapper"])
        .env("HOME", &home)
        .env("SHELL", "/usr/bin/fish")
        .assert()
        .success()
        .stdout(predicates::str::contains("bash/zsh shell function"));

    let rc = fs::read_to_string(home.join(".config/fish/config.fish")).expect("read fish config");
    assert!(rc.contains("git stk completions fish"), "{rc}");
    assert!(
        !rc.contains("stk() {"),
        "bash syntax must not land in fish: {rc}"
    );
}

#[test]
fn the_bash_block_loads_silently_and_defines_stk() {
    // The worst thing an rc block can do is fail or chatter on every shell
    // start, so run what setup wrote through a real bash. git-stk is deliberately
    // absent from PATH here: every line is guarded, so this must stay silent.
    let Ok(bash) = which_bash() else {
        eprintln!("no bash available; skipping");
        return;
    };
    let repo = TestRepo::new();
    let home = fake_home(&repo);

    repo.stack()
        .args(["setup", "--yes", "--wrapper"])
        .env("HOME", &home)
        .env("SHELL", "/bin/bash")
        .assert()
        .success();

    let rc = home.join(".bashrc");
    let output = std::process::Command::new(bash)
        .args([
            "--noprofile",
            "--norc",
            "-c",
            &format!("source {}; type -t stk", rc.display()),
        ])
        .env("PATH", "/nonexistent")
        .output()
        .expect("run bash");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "sourcing failed: {stderr}");
    assert_eq!(
        stdout.trim(),
        "function",
        "stk not defined: {stdout}{stderr}"
    );
    assert!(
        stderr.is_empty(),
        "the block must not print anything at shell start: {stderr}"
    );
}

fn which_bash() -> Result<std::path::PathBuf, ()> {
    std::env::split_paths(&std::env::var_os("PATH").ok_or(())?)
        .map(|dir| dir.join("bash"))
        .find(|path| path.is_file())
        .ok_or(())
}

#[test]
fn setup_non_interactive_skips_completions_but_installs_man_page() {
    let repo = TestRepo::new();
    let home = repo.path().join("home");
    fs::create_dir_all(&home).expect("create fake home");

    // No TTY and no --yes (the curl|bash case): setup must not show a prompt it
    // can't answer - it skips completions cleanly and prints the manual line.
    repo.stack()
        .args(["setup"])
        .env("HOME", &home)
        .env("SHELL", "/bin/bash")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "non-interactive shell; skipped completion setup",
        ))
        .stdout(predicates::str::contains(
            "source <(git stk completions bash)",
        ));

    assert!(home.join(".local/share/man/man1/git-stk.1").exists());
    assert!(!home.join(".bashrc").exists());
}

#[test]
fn setup_unknown_shell_prints_manual_hint() {
    let repo = TestRepo::new();
    let home = repo.path().join("home");
    fs::create_dir_all(&home).expect("create fake home");

    repo.stack()
        .args(["setup", "--yes"])
        .env("HOME", &home)
        .env("SHELL", "/bin/tcsh")
        // An empty PATH so the PowerShell fallback finds nothing (CI runners
        // ship pwsh); the unknown shell then falls through to the hint.
        .env("PATH", &home)
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "could not detect a supported shell",
        ));

    assert!(!home.join(".bashrc").exists());
}

#[test]
fn setup_respects_xdg_data_home_for_man_page() {
    let repo = TestRepo::new();
    let home = repo.path().join("home");
    let data = repo.path().join("xdg-data");
    fs::create_dir_all(&home).expect("create fake home");

    repo.stack()
        .args(["setup", "--yes"])
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &data)
        .env("SHELL", "/bin/bash")
        .assert()
        .success();

    assert!(data.join("man/man1/git-stk.1").exists());
    assert!(!home.join(".local/share/man/man1/git-stk.1").exists());
}

#[test]
fn setup_refresh_installs_man_page_without_touching_rc() {
    let repo = TestRepo::new();
    let home = repo.path().join("home");
    fs::create_dir_all(&home).expect("create fake home");

    repo.stack()
        .args(["setup", "--refresh"])
        .env("HOME", &home)
        .env_remove("XDG_DATA_HOME")
        .env("SHELL", "/bin/bash")
        .assert()
        .success()
        .stdout(predicates::str::contains("installed man page"))
        .stdout(predicates::str::contains(
            "bash completions are not configured; run `git stk setup`",
        ));

    assert!(home.join(".local/share/man/man1/git-stk.1").exists());
    assert!(!home.join(".bashrc").exists());
}

/// Build a fake install footprint (bashrc completion block, man page, config
/// dir) under `home`/`config`, and return the env a uninstall run needs.
fn plant_install(home: &std::path::Path, config: &std::path::Path) {
    fs::create_dir_all(home.join(".local/share/man/man1")).expect("man dir");
    fs::create_dir_all(config.join("git-stk")).expect("config dir");
    fs::write(
        home.join(".bashrc"),
        "export PATH=/x\n\n# added by git-stk setup\ncommand -v git-stk >/dev/null && source <(git stk completions bash)\n",
    )
    .expect("write bashrc");
    fs::write(home.join(".local/share/man/man1/git-stk.1"), "manpage").expect("man page");
    fs::write(config.join("git-stk/update-check"), "checked=1\n").expect("receipt dir");
}

#[test]
fn uninstall_removes_the_setup_and_installer_footprint() {
    let repo = TestRepo::new();
    let home = repo.path().join("home");
    let config = repo.path().join("config");
    plant_install(&home, &config);

    repo.stack()
        .args(["uninstall", "-y"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("SHELL", "/bin/bash")
        .assert()
        .success()
        .stdout(predicates::str::contains("removed bash completion line"))
        .stdout(predicates::str::contains("removed man page"))
        .stdout(predicates::str::contains(
            "the git-stk binary is left in place",
        ));

    // The completion block is gone, the rest of the rc intact.
    assert_eq!(
        fs::read_to_string(home.join(".bashrc")).expect("read bashrc"),
        "export PATH=/x\n"
    );
    assert!(!home.join(".local/share/man/man1/git-stk.1").exists());
    assert!(!config.join("git-stk").exists());
}

#[test]
fn uninstall_dry_run_removes_nothing() {
    let repo = TestRepo::new();
    let home = repo.path().join("home");
    let config = repo.path().join("config");
    plant_install(&home, &config);

    repo.stack()
        .args(["uninstall", "--dry-run"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("SHELL", "/bin/bash")
        .assert()
        .success()
        .stdout(predicates::str::contains("dry run: nothing was removed"));

    // Everything still there.
    assert!(
        fs::read_to_string(home.join(".bashrc"))
            .expect("read bashrc")
            .contains("# added by git-stk setup")
    );
    assert!(home.join(".local/share/man/man1/git-stk.1").exists());
    assert!(config.join("git-stk").exists());
}

#[test]
fn setup_refresh_stays_quiet_when_completions_are_configured() {
    let repo = TestRepo::new();
    let home = repo.path().join("home");
    fs::create_dir_all(&home).expect("create fake home");
    fs::write(
        home.join(".bashrc"),
        "# added by git-stk setup\ncommand -v git-stk >/dev/null && source <(git stk completions bash)\n",
    )
    .expect("write bashrc");

    repo.stack()
        .args(["setup", "--refresh"])
        .env("HOME", &home)
        .env("SHELL", "/bin/bash")
        .assert()
        .success()
        .stdout(predicates::str::contains("completions are not configured").not());
}
