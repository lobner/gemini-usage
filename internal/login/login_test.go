package login

import (
	"os"
	"path/filepath"
	"testing"
)

func TestBundleFor(t *testing.T) {
	tests := []struct {
		exe  string
		want string
	}{
		{"/Applications/Gemini Usage.app/Contents/MacOS/geminiusage", "/Applications/Gemini Usage.app"},
		{"/Users/x/tools/My App.app/Contents/MacOS/bin", "/Users/x/tools/My App.app"},
		{"/Users/x/workspace/gemini-usage/geminiusage", ""}, // plain `go build`
		{"/tmp/go-build123/b001/exe/main", ""},              // `go run .`
		{"/Applications/Thing.app/Contents/Resources/geminiusage", ""},
		{"/Applications/Thing/Contents/MacOS/geminiusage", ""}, // not a bundle
	}
	for _, tt := range tests {
		if got := bundleFor(tt.exe); got != tt.want {
			t.Errorf("bundleFor(%q) = %q, want %q", tt.exe, got, tt.want)
		}
	}
}

// TestAnswered drives the state file against a throwaway HOME, so the
// developer's own Application Support directory stays untouched.
func TestAnswered(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)

	if Answered() {
		t.Fatal("Answered() before RecordAnswer()")
	}
	if Answer() {
		t.Error("Answer() = true before RecordAnswer()")
	}
	if err := RecordAnswer(true); err != nil {
		t.Fatalf("RecordAnswer: %v", err)
	}
	if !Answered() {
		t.Error("Answered() = false after RecordAnswer()")
	}
	if !Answer() {
		t.Error("Answer() = false after RecordAnswer(true)")
	}

	p := filepath.Join(home, "Library", "Application Support", stateDir, "launch-at-login")
	data, err := os.ReadFile(p)
	if err != nil {
		t.Fatalf("read state: %v", err)
	}
	if got := string(data); got != "registered\n" {
		t.Errorf("state = %q, want %q", got, "registered\n")
	}

	if err := RecordAnswer(false); err != nil {
		t.Fatalf("RecordAnswer: %v", err)
	}
	if data, _ = os.ReadFile(p); string(data) != "declined\n" {
		t.Errorf("state = %q, want %q", data, "declined\n")
	}
	if Answer() {
		t.Error("Answer() = true after RecordAnswer(false)")
	}
	if !Answered() {
		t.Error("Answered() = false after RecordAnswer(false); declining still counts as answered")
	}
}

// TestSupported pins the SMAppService wiring: the cgo bridge must link and
// answer, and on any macOS this app can be developed on it should say yes.
func TestSupported(t *testing.T) {
	if !Supported() {
		t.Error("Supported() = false; SMAppService should be present on macOS 13+")
	}
}
