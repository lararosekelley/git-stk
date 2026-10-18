use anyhow::Result;

use crate::browser;
use crate::commands::Run;
use crate::style;

const DONATE_URL: &str = "https://ko-fi.com/sayonara_lara";

/// Open the page to support git-stk's development in your browser.
#[derive(Debug, clap::Args)]
pub struct Donate;

impl Run for Donate {
    fn run(self) -> Result<()> {
        anstream::println!(
            "thank you for supporting git-stk! opening {}",
            style::dim(DONATE_URL)
        );
        if let Err(error) = browser::open(DONATE_URL) {
            anstream::eprintln!(
                "could not open a browser ({error:#}); open the address above instead"
            );
        }
        Ok(())
    }
}
