package notify

import (
	"errors"
	"os/exec"
	"testing"
)

// TestUserCancelled covers the reason Confirm keys off the error code: the
// message wording differs between systems, and reading it instead once made a
// declined dialog look like a failed one.
func TestUserCancelled(t *testing.T) {
	tests := []struct {
		name   string
		stderr string
		want   bool
	}{
		{"cancelled, two l's", "0:153: execution error: User cancelled. (-128)", true},
		{"canceled, one l", "0:132: execution error: User canceled. (-128)", true},
		{"localized wording", "0:99: execution error: Brugeren annullerede. (-128)", true},
		{"some other failure", "0:12: syntax error: Expected end of line. (-2741)", false},
		{"empty", "", false},
	}
	for _, tt := range tests {
		err := error(&exec.ExitError{Stderr: []byte(tt.stderr)})
		if got := userCancelled(err); got != tt.want {
			t.Errorf("%s: userCancelled(%q) = %v, want %v", tt.name, tt.stderr, got, tt.want)
		}
	}

	if userCancelled(errors.New("osascript: not found")) {
		t.Error("userCancelled(non-exit error) = true, want false")
	}
	if userCancelled(nil) {
		t.Error("userCancelled(nil) = true, want false")
	}
}

func TestQuote(t *testing.T) {
	tests := []struct{ in, want string }{
		{`plain`, `"plain"`},
		{`say "hi"`, `"say \"hi\""`},
		{`back\slash`, `"back\\slash"`},
		{"two\nlines", `"two lines"`},
	}
	for _, tt := range tests {
		if got := quote(tt.in); got != tt.want {
			t.Errorf("quote(%q) = %s, want %s", tt.in, got, tt.want)
		}
	}
}
