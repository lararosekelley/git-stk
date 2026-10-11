//! Opening a local file with the platform's default handler - the one place
//! git-stk launches the system opener.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Open `path` in the default browser (or whatever handles its type).
pub fn open(path: &Path) -> Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        // Not `cmd /C start`: cmd.exe would interpret `&`, `^`, or `%` in the path.
        let mut command = Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else {
        Command::new("xdg-open")
    };
    let status = command
        .arg(path)
        .status()
        .context("failed to launch the opener")?;
    if !status.success() {
        bail!("the opener exited with {status}");
    }
    Ok(())
}
