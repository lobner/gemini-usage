// Package login controls whether the app opens automatically at login.
//
// It registers the app itself as a login item through ServiceManagement
// (SMAppService, macOS 13+), which is what puts it under System Settings →
// General → Login Items → "Open at Login". A LaunchAgent in
// ~/Library/LaunchAgents would work too, but launchd jobs are classified as
// background items and show up in the "App Background Activity" list instead.
//
// Whether the offer has been made is remembered here rather than asked of the
// system: SMAppService reports status "enabled" for a bundle that has never been
// registered — it only says "notRegistered" once something has explicitly
// unregistered it — so its status cannot tell a first launch from a registered
// one.
package login

import (
	"os"
	"path/filepath"
	"strings"
)

// stateDir is the Application Support directory holding our one bit of state,
// whose contents is one of the answer constants below.
const (
	stateDir         = "dk.biq.geminiusage"
	answerRegistered = "registered"
	answerDeclined   = "declined"
)

// BundlePath returns the .app bundle the running executable lives in, or "" when
// the binary is not bundled (`go run .`, `go build && ./geminiusage`). Login
// items are bundles, so there is nothing to register in that case.
func BundlePath() string {
	exe, err := os.Executable()
	if err != nil {
		return ""
	}
	exe, err = filepath.EvalSymlinks(exe)
	if err != nil {
		return ""
	}
	return bundleFor(exe)
}

// bundleFor maps an executable path to its enclosing .app, or "" if it has the
// wrong shape: …/Gemini Usage.app/Contents/MacOS/geminiusage.
func bundleFor(exe string) string {
	macOS := filepath.Dir(exe)
	contents := filepath.Dir(macOS)
	bundle := filepath.Dir(contents)
	if filepath.Base(macOS) != "MacOS" || filepath.Base(contents) != "Contents" ||
		!strings.HasSuffix(bundle, ".app") {
		return ""
	}
	return bundle
}

// Answered reports whether the choice has been made already, either way, so the
// offer is made once rather than on every launch.
func Answered() bool {
	p, err := answerPath()
	if err != nil {
		return false
	}
	_, err = os.Stat(p)
	return err == nil
}

// Answer reports the recorded choice: true when we last registered the app as a
// login item, false when it was declined, switched off, or never asked.
func Answer() bool {
	p, err := answerPath()
	if err != nil {
		return false
	}
	data, err := os.ReadFile(p)
	if err != nil {
		return false
	}
	return strings.TrimSpace(string(data)) == answerRegistered
}

// RecordAnswer remembers the current choice, both to stop re-asking and as half
// of what Enabled reports.
func RecordAnswer(enabled bool) error {
	p, err := answerPath()
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		return err
	}
	answer := answerDeclined
	if enabled {
		answer = answerRegistered
	}
	return os.WriteFile(p, []byte(answer+"\n"), 0o644)
}

func answerPath() (string, error) {
	dir, err := os.UserConfigDir() // ~/Library/Application Support
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, stateDir, "launch-at-login"), nil
}
