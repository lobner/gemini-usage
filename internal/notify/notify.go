// Package notify puts things in front of the user — Notification Centre banners
// and the occasional dialog — via osascript, which needs no app bundle. (When
// the binary is wrapped in a .app the banner is attributed to that app instead
// of to "Script Editor".)
package notify

import (
	"errors"
	"os/exec"
	"strings"
)

// Banner shows a notification with the given title and body. Errors are returned
// but are safe to ignore — a missing notification must never crash the tracker.
func Banner(title, body string) error {
	script := "display notification " + quote(body) + " with title " + quote(title)
	return exec.Command("osascript", "-e", script).Run()
}

// Confirm shows a two-button dialog and reports whether the user picked the
// confirming one. Pressing Escape counts as declining, like the cancel button.
// It blocks until the user answers, so call it off the poll goroutine.
func Confirm(title, message, confirmLabel, cancelLabel string) (bool, error) {
	script := "display dialog " + quote(message) +
		" with title " + quote(title) +
		" buttons {" + quote(cancelLabel) + ", " + quote(confirmLabel) + "}" +
		" default button " + quote(confirmLabel) +
		" cancel button " + quote(cancelLabel)
	// No "with icon": osascript has no bundle of its own, so the note icon
	// resolves to a generic folder graphic rather than to anything meaningful.

	out, err := exec.Command("osascript", "-e", script).Output()
	if err != nil {
		if userCancelled(err) {
			return false, nil
		}
		return false, err
	}
	return strings.Contains(string(out), "button returned:"+confirmLabel), nil
}

// userCancelled reports whether osascript failed because the user dismissed the
// dialog — either by clicking the cancel button or by pressing Escape, both of
// which raise AppleScript error -128. It matches the code rather than the
// message, whose wording varies by system ("canceled" vs "cancelled") and is
// localized.
func userCancelled(err error) bool {
	var ee *exec.ExitError
	return errors.As(err, &ee) && strings.Contains(string(ee.Stderr), "(-128)")
}

// quote turns s into a safe AppleScript double-quoted string literal.
func quote(s string) string {
	r := strings.NewReplacer(
		`\`, `\\`,
		`"`, `\"`,
		"\n", " ",
		"\r", " ",
	)
	return `"` + r.Replace(s) + `"`
}
