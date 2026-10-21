//! Controls whether the app opens automatically at login.
//!
//! It registers the app itself as a login item through ServiceManagement
//! (SMAppService, macOS 13+), which puts it under System Settings → General →
//! Login Items → "Open at Login". A LaunchAgent in ~/Library/LaunchAgents also
//! works, but macOS classifies launchd jobs as background items and shows them
//! in the "App Background Activity" list instead.
//!
//! This module records whether the offer has been made, because the system
//! cannot tell: SMAppService reports status "enabled" for a bundle that has
//! never been registered. It only says "notRegistered" after something has
//! explicitly unregistered it, so its status cannot tell a first launch from a
//! registered one.

use std::ffi::OsString;
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use objc2::available;
use objc2_service_management::{SMAppService, SMAppServiceStatus};

/// The Application Support directory that holds our one bit of state. The
/// file in it contains one of the answer constants below. It is the bundle id,
/// which is the identity of the app to macOS: changing it loses every user's
/// recorded answer.
const STATE_DIR: &str = "dk.biq.geminiusage";
const ANSWER_REGISTERED: &str = "registered";
const ANSWER_DECLINED: &str = "declined";

/// macOS 12 or earlier: there is no SMAppService.
const STATUS_UNSUPPORTED: isize = -1;

/// Returns the .app bundle that the running executable is in, or `None` when
/// the binary is not in a bundle (`cargo run`, `./target/debug/geminiusage`).
/// Login items are bundles, so then there is nothing to register.
pub fn bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(exe).ok()?;
    bundle_for(&exe)
}

/// Maps an executable path to its .app, or `None` if it has the wrong shape:
/// …/Gemini Usage.app/Contents/MacOS/geminiusage.
fn bundle_for(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    if macos.file_name()? != "MacOS"
        || contents.file_name()? != "Contents"
        || !bundle.to_string_lossy().ends_with(".app")
    {
        return None;
    }
    Some(bundle.to_path_buf())
}

/// Reports whether the choice has been made already, either way, so the offer
/// is made once and not on every launch.
pub fn answered() -> bool {
    home().is_some_and(|h| answered_in(&h))
}

/// Reports the recorded choice: true when we last registered the app as a
/// login item, false when it was declined, switched off, or never asked.
pub fn answer() -> bool {
    home().is_some_and(|h| answer_in(&h))
}

/// Remembers the current choice, both to stop asking again and as half of what
/// `enabled` reports.
pub fn record_answer(enabled: bool) -> std::io::Result<()> {
    let home = home().ok_or_else(|| std::io::Error::other("$HOME is not defined"))?;
    record_answer_in(&home, enabled)
}

/// $HOME, as Go's os.UserConfigDir reads it.
fn home() -> Option<OsString> {
    std::env::var_os("HOME").filter(|h| !h.is_empty())
}

fn answer_path(home: &OsString) -> PathBuf {
    Path::new(home)
        .join("Library")
        .join("Application Support")
        .join(STATE_DIR)
        .join("launch-at-login")
}

fn answered_in(home: &OsString) -> bool {
    std::fs::metadata(answer_path(home)).is_ok()
}

fn answer_in(home: &OsString) -> bool {
    std::fs::read_to_string(answer_path(home)).is_ok_and(|s| s.trim() == ANSWER_REGISTERED)
}

fn record_answer_in(home: &OsString, enabled: bool) -> std::io::Result<()> {
    let p = answer_path(home);
    if let Some(dir) = p.parent() {
        DirBuilder::new().recursive(true).mode(0o755).create(dir)?;
    }
    let answer = if enabled {
        ANSWER_REGISTERED
    } else {
        ANSWER_DECLINED
    };
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(p)?;
    f.write_all(format!("{answer}\n").as_bytes())
}

// ---- SMAppService ----

fn status() -> isize {
    if available!(macos = 13.0) {
        // SAFETY: SMAppService exists on macOS 13 and later, checked above.
        unsafe { SMAppService::mainAppService().status().0 }
    } else {
        STATUS_UNSUPPORTED
    }
}

/// Reports whether this macOS can register the app as a login item.
pub fn supported() -> bool {
    status() != STATUS_UNSUPPORTED
}

/// Reports whether the app currently opens at login.
///
/// It combines what the system says with what we recorded, because neither is
/// enough alone: SMAppService reports "enabled" for a bundle that has never
/// been registered, so a positive status is only believable after we have
/// registered it. And the user can switch the item off in System Settings
/// without us, which the status does show.
pub fn enabled() -> bool {
    status() == SMAppServiceStatus::Enabled.0 && answer()
}

/// Registers the app to open at login. macOS remembers the bundle by the path
/// it is registered from, so register the copy that is meant to stay put.
pub fn enable() -> Result<(), String> {
    change(true)
}

/// Unregisters the login item.
pub fn disable() -> Result<(), String> {
    change(false)
}

/// Reports whether macOS holds the login item back until the user switches it
/// on, which happens when they have switched it off in System Settings before.
pub fn needs_approval() -> bool {
    status() == SMAppServiceStatus::RequiresApproval.0
}

fn change(enable: bool) -> Result<(), String> {
    if !available!(macos = 13.0) {
        return Err("requires macOS 13 or later".into());
    }
    // SAFETY: SMAppService exists on macOS 13 and later, checked above.
    let result = unsafe {
        let svc = SMAppService::mainAppService();
        if enable {
            svc.registerAndReturnError()
        } else {
            svc.unregisterAndReturnError()
        }
    };
    result.map_err(|err| {
        let msg = err.localizedDescription().to_string();
        if msg.is_empty() {
            "the login item could not be changed".into()
        } else {
            msg
        }
    })
}

/// Makes the system match the recorded intent, and reports whether the app is
/// registered afterwards. Call it at startup when `answer` is true.
///
/// It is necessary because the system cannot be asked "is this bundle
/// registered?": SMAppService answers "enabled" for one that never was. So
/// when our record says we registered and the system gives that ambiguous
/// answer, register again. That does nothing if it was already true, and it
/// repairs a stale record or an app that has moved since (macOS remembers the
/// path it was registered from).
///
/// A status of notRegistered or requiresApproval means someone switched the
/// item off outside the app, most likely in System Settings. That is a decision
/// to respect, not to undo, so it is left alone and reported as not registered.
pub fn reconcile() -> bool {
    if !answer() {
        return false;
    }
    if status() != SMAppServiceStatus::Enabled.0 {
        return false;
    }
    enable().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_for_shapes() {
        let cases = [
            (
                "/Applications/Gemini Usage.app/Contents/MacOS/geminiusage",
                Some("/Applications/Gemini Usage.app"),
            ),
            (
                "/Users/x/tools/My App.app/Contents/MacOS/bin",
                Some("/Users/x/tools/My App.app"),
            ),
            (
                "/Users/x/workspace/gemini-usage/target/debug/geminiusage",
                None,
            ), // cargo build
            ("/Users/x/workspace/gemini-usage/geminiusage", None),
            (
                "/Applications/Thing.app/Contents/Resources/geminiusage",
                None,
            ),
            ("/Applications/Thing/Contents/MacOS/geminiusage", None), // not a bundle
        ];
        for (exe, want) in cases {
            assert_eq!(
                bundle_for(Path::new(exe)),
                want.map(PathBuf::from),
                "bundle_for({exe:?})"
            );
        }
    }

    /// Drives the state file against a throwaway home directory, so the
    /// developer's own Application Support directory stays untouched.
    #[test]
    fn answered_round_trip() {
        let dir = std::env::temp_dir().join(format!("geminiusage-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.clone().into_os_string();

        assert!(!answered_in(&home), "answered before record_answer");
        assert!(!answer_in(&home), "answer() = true before record_answer");

        record_answer_in(&home, true).expect("record_answer");
        assert!(answered_in(&home));
        assert!(answer_in(&home));
        let p = answer_path(&home);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "registered\n");

        record_answer_in(&home, false).expect("record_answer");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "declined\n");
        assert!(!answer_in(&home));
        assert!(answered_in(&home), "declining still counts as answered");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pins the SMAppService wiring: the bindings must link and answer, and on
    /// any macOS this app can be developed on, the answer should be yes.
    #[test]
    fn supported_on_this_mac() {
        assert!(supported(), "SMAppService should be present on macOS 13+");
    }
}
