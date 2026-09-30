//! Copying text to the system clipboard: pbcopy locally, OSC 52 over SSH.

use std::io::{self, Write};
use std::process::{Command, Stdio};

/// A way to reach the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Pbcopy,
    Osc52,
}

impl Method {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Pbcopy => "pbcopy",
            Self::Osc52 => "OSC 52",
        }
    }
}

/// Largest text sent over OSC 52; many terminals drop longer sequences without a word.
pub const OSC52_LIMIT: usize = 100_000;

/// What to tell the user after `method` took `size` of text. OSC 52 only hands the text to
/// the terminal, which may still refuse it.
#[must_use]
pub fn summary(method: Method, size: &str) -> String {
    match method {
        Method::Pbcopy => format!("copied {size} (pbcopy)"),
        Method::Osc52 => format!("sent {size} to the terminal (OSC 52)"),
    }
}

/// Methods to try, in order: OSC 52 first over SSH (pbcopy would copy on the wrong machine).
#[must_use]
pub fn strategy(ssh: bool) -> [Method; 2] {
    if ssh {
        [Method::Osc52, Method::Pbcopy]
    } else {
        [Method::Pbcopy, Method::Osc52]
    }
}

/// Standard base64 (RFC 4648) with padding.
#[must_use]
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            let sextet = (group >> (18 - 6 * i)) & 0x3F;
            out.push(if i <= chunk.len() {
                char::from(ALPHABET[sextet as usize])
            } else {
                '='
            });
        }
    }
    out
}

/// The OSC 52 sequence that asks the terminal to set the clipboard.
#[must_use]
pub fn osc52(text: &str) -> String {
    format!("\u{1b}]52;c;{}\u{7}", base64(text.as_bytes()))
}

/// Copies `text`, trying each method of the current [`strategy`].
///
/// # Errors
/// The last method's failure when none succeeds.
pub fn copy(text: &str) -> Result<Method, String> {
    let ssh = std::env::var_os("SSH_TTY").is_some();
    let mut last = String::new();
    for method in strategy(ssh) {
        match run(method, text) {
            Ok(()) => return Ok(method),
            Err(err) => last = format!("{}: {err}", method.name()),
        }
    }
    Err(last)
}

fn run(method: Method, text: &str) -> io::Result<()> {
    match method {
        Method::Pbcopy => pbcopy(text),
        Method::Osc52 => {
            let sequence = osc52_checked(text)?;
            let mut out = io::stdout().lock();
            out.write_all(sequence.as_bytes())?;
            out.flush()
        }
    }
}

/// [`osc52`], unless `text` is over [`OSC52_LIMIT`].
fn osc52_checked(text: &str) -> io::Result<String> {
    if text.len() > OSC52_LIMIT {
        return Err(io::Error::other(format!(
            "{} bytes is over the {OSC52_LIMIT}-byte OSC 52 limit",
            text.len()
        )));
    }
    Ok(osc52(text))
}

/// `pbcopy` reads UTF-8 only when `LANG` says so, and a terminal's locale may not.
fn pbcopy_command() -> Command {
    let mut command = Command::new("pbcopy");
    command.env("LANG", "en_US.UTF-8");
    command
}

fn pbcopy(text: &str) -> io::Result<()> {
    let mut child = pbcopy_command()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("no stdin"))?
        .write_all(text.as_bytes())?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("exited with {status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in vectors {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0x00]), "//4A");
    }

    #[test]
    fn osc52_frames_the_payload() {
        assert_eq!(osc52("hi"), "\u{1b}]52;c;aGk=\u{7}");
    }

    #[test]
    fn pbcopy_runs_with_a_utf8_locale() {
        let command = pbcopy_command();
        let lang = command.get_envs().find(|(k, _)| *k == "LANG");
        assert_eq!(lang.and_then(|(_, v)| v), Some("en_US.UTF-8".as_ref()));
    }

    #[test]
    fn osc52_refuses_text_over_the_limit() {
        assert!(osc52_checked(&"a".repeat(OSC52_LIMIT)).is_ok());
        let err = osc52_checked(&"a".repeat(OSC52_LIMIT + 1)).unwrap_err();
        assert!(err.to_string().contains("OSC 52 limit"), "{err}");
    }

    #[test]
    fn osc52_is_reported_as_sent_not_copied() {
        assert_eq!(summary(Method::Pbcopy, "5 B"), "copied 5 B (pbcopy)");
        assert_eq!(
            summary(Method::Osc52, "5 B"),
            "sent 5 B to the terminal (OSC 52)"
        );
    }

    #[test]
    fn ssh_prefers_osc52() {
        assert_eq!(strategy(true), [Method::Osc52, Method::Pbcopy]);
        assert_eq!(strategy(false), [Method::Pbcopy, Method::Osc52]);
    }
}
