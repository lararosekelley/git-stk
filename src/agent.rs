//! Running the agent command a tour is planned with (`stk.tourAgent`) - the
//! one place git-stk starts it. The command is the user's own harness, such
//! as `claude -p` or `llm -m <model>`, so git-stk never holds a key for it.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// How long one request may run before the agent is stopped.
const TIMEOUT: Duration = Duration::from_secs(600);

pub struct Agent {
    words: Vec<String>,
}

impl Agent {
    /// The agent `command` names, split into words as a shell would split a
    /// simple command line, without running a shell.
    pub fn parse(command: &str) -> Result<Self> {
        let words = split_words(command)?;
        if words.is_empty() {
            bail!("the agent command is empty");
        }
        Ok(Self { words })
    }

    /// The command as written, for showing who planned a tour.
    pub fn display(&self) -> String {
        self.words
            .iter()
            .map(|word| {
                if word.is_empty() || word.contains(char::is_whitespace) {
                    format!("'{word}'")
                } else {
                    word.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Send `request` on the agent's stdin and return what it prints.
    pub fn ask(&self, request: &str) -> Result<String> {
        let mut child = Command::new(&self.words[0])
            .args(&self.words[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to run the agent `{}`", self.display()))?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let request = request.to_owned();
        // An agent that exits without reading all its input closes the pipe;
        // its exit status says how it went.
        let writer = thread::spawn(move || {
            let _ = stdin.write_all(request.as_bytes());
        });
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let reader = thread::spawn(move || {
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            text
        });
        let errors = thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });

        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "the agent `{}` took longer than {} minutes",
                    self.display(),
                    TIMEOUT.as_secs() / 60
                );
            }
            thread::sleep(Duration::from_millis(100));
        };
        let _ = writer.join();
        let output = reader.join().unwrap_or_default();
        let errors = errors.join().unwrap_or_default();
        if !status.success() {
            let detail = errors.trim();
            bail!(
                "the agent `{}` exited with {status}{}",
                self.display(),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            );
        }
        Ok(output)
    }
}

/// `line` split into words the way a POSIX shell splits a simple command:
/// on unquoted whitespace, with single quotes taken literally, double quotes
/// allowing `\"` and `\\`, and a backslash escaping the next character
/// outside quotes. Expansions and operators are not interpreted.
fn split_words(line: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => word.push(ch),
                        None => bail!("unclosed ' in the agent command"),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(next @ ('"' | '\\')) => word.push(next),
                            Some(next) => {
                                word.push('\\');
                                word.push(next);
                            }
                            None => bail!("unclosed \" in the agent command"),
                        },
                        Some(ch) => word.push(ch),
                        None => bail!("unclosed \" in the agent command"),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            ch if ch.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            ch => {
                in_word = true;
                word.push(ch);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_split_like_a_shell() {
        assert_eq!(
            split_words(r#"claude -p --model "opus 5" --append 'a "b"' x\ y"#).unwrap(),
            [
                "claude", "-p", "--model", "opus 5", "--append", r#"a "b""#, "x y"
            ]
        );
        assert_eq!(split_words("  llm   -m  m ").unwrap(), ["llm", "-m", "m"]);
        assert_eq!(split_words(r#"a "" b"#).unwrap(), ["a", "", "b"]);
        assert!(split_words("a 'b").is_err());
        assert!(split_words(r#"a "b"#).is_err());
    }

    #[test]
    fn the_display_quotes_what_needs_it() {
        let agent = Agent::parse(r#"claude -p --model "opus 5""#).unwrap();
        assert_eq!(agent.display(), "claude -p --model 'opus 5'");
    }

    #[cfg(unix)]
    #[test]
    fn an_agent_answers_on_stdout_and_fails_with_its_stderr() {
        let echo = Agent::parse("cat").unwrap();
        assert_eq!(echo.ask("hello").unwrap(), "hello");
        let failing = Agent::parse("sh -c 'echo nope >&2; exit 3'").unwrap();
        let error = failing.ask("").unwrap_err().to_string();
        assert!(error.contains("nope"), "{error}");
    }
}
