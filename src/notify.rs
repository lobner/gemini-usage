//! Puts things in front of the user - Notification Centre banners and the
//! occasional dialog - via osascript, which needs no app bundle. (When the
//! binary is in a .app, the banner shows that app instead of "Script Editor".)

use std::process::{Command, Output, Stdio};

/// The absolute path, because an app started from Finder or at login gets a
/// minimal PATH.
const OSASCRIPT: &str = "/usr/bin/osascript";

/// Shows a notification with the given title and body. The error is safe to
/// ignore: a missing notification must never stop the app.
pub fn banner(title: &str, body: &str) -> Result<(), String> {
    let script = format!(
        "display notification {} with title {}",
        quote(body),
        quote(title)
    );
    let status = Command::new(OSASCRIPT)
        .args(["-e", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(status.to_string())
    }
}

/// Shows a two-button dialog and reports whether the user picked the
/// confirming one. Escape counts as declining, as the cancel button does. It
/// blocks until the user answers, so do not call it on the poll thread.
pub fn confirm(
    title: &str,
    message: &str,
    confirm_label: &str,
    cancel_label: &str,
) -> Result<bool, String> {
    let script = format!(
        "display dialog {} with title {} buttons {{{}, {}}} default button {} cancel button {}",
        quote(message),
        quote(title),
        quote(cancel_label),
        quote(confirm_label),
        quote(confirm_label),
        quote(cancel_label),
    );
    // No "with icon": osascript has no bundle of its own, so the note icon
    // resolves to a generic folder graphic, not to anything meaningful.

    let out = Command::new(OSASCRIPT)
        .args(["-e", &script])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        if user_cancelled(&out) {
            return Ok(false);
        }
        return Err(format!(
            "{}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).contains(&format!("button returned:{confirm_label}")))
}

/// Reports whether osascript failed because the user dismissed the dialog -
/// with the cancel button or with Escape, both of which raise AppleScript error
/// -128. It matches the code, not the message: the wording is localized and
/// differs between systems ("canceled" and "cancelled").
fn user_cancelled(out: &Output) -> bool {
    !out.status.success() && String::from_utf8_lossy(&out.stderr).contains("(-128)")
}

/// Turns `s` into a safe AppleScript double-quoted string literal.
fn quote(s: &str) -> String {
    let mut q = String::with_capacity(s.len() + 2);
    q.push('"');
    for c in s.chars() {
        match c {
            '\\' => q.push_str("\\\\"),
            '"' => q.push_str("\\\""),
            '\n' | '\r' => q.push(' '),
            c => q.push(c),
        }
    }
    q.push('"');
    q
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    fn failed(stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(1 << 8), // exit code 1
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// The reason confirm keys off the error code: the message wording differs
    /// between systems, and reading it once made a declined dialog look like a
    /// failed one.
    #[test]
    fn user_cancelled_matches_the_code() {
        let cases = [
            ("0:153: execution error: User cancelled. (-128)", true),
            ("0:132: execution error: User canceled. (-128)", true),
            ("0:99: execution error: Brugeren annullerede. (-128)", true),
            ("0:12: syntax error: Expected end of line. (-2741)", false),
            ("", false),
        ];
        for (stderr, want) in cases {
            assert_eq!(user_cancelled(&failed(stderr)), want, "{stderr:?}");
        }

        let ok = Output {
            status: ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: b"(-128)".to_vec(),
        };
        assert!(!user_cancelled(&ok), "a successful run is not a cancel");
    }

    #[test]
    fn quote_escapes() {
        let cases = [
            ("plain", r#""plain""#),
            (r#"say "hi""#, r#""say \"hi\"""#),
            (r"back\slash", r#""back\\slash""#),
            ("two\nlines", r#""two lines""#),
        ];
        for (input, want) in cases {
            assert_eq!(quote(input), want);
        }
    }
}
