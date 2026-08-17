package icon

import (
	"bytes"
	"image/png"
	"testing"
)

func TestRedDotPNGDecodes(t *testing.T) {
	b := RedDotPNG()
	img, err := png.Decode(bytes.NewReader(b))
	if err != nil {
		t.Fatalf("decode: %v", err)
	}
	if got := img.Bounds().Dx(); got != canvas {
		t.Errorf("width = %d, want %d", got, canvas)
	}

	// Opaque red in the middle, fully transparent in the corner.
	r, g, bl, a := img.At(canvas/2, canvas/2).RGBA()
	if a == 0 || r <= g || r <= bl {
		t.Errorf("centre pixel = (%d,%d,%d,%d), want an opaque red", r, g, bl, a)
	}
	if _, _, _, a := img.At(0, 0).RGBA(); a != 0 {
		t.Errorf("corner alpha = %d, want 0", a)
	}
}

func TestRedDotPNGIsCached(t *testing.T) {
	if &RedDotPNG()[0] != &RedDotPNG()[0] {
		t.Errorf("expected the encoded PNG to be computed once and shared")
	}
}
