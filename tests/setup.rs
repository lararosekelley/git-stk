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

    repo.stack()
        .args(["setup"])
        .env("HOME", &home)
        .env_remove("XDG_DATA_HOME")
        .env("SHELL", "/bin/bash")
        .write_stdin("y\n")
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

#[test]
fn setup_declining_prompt_skips_rc_but_installs_man_page() {
    let repo = TestRepo::new();
    let home = repo.path().join("home");
    fs::create_dir_all(&home).expect("create fake home");

    repo.stack()
        .args(["setup"])
        .env("HOME", &home)
        .env("SHELL", "/bin/bash")
        .write_stdin("n\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("skipped completion setup"))
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
