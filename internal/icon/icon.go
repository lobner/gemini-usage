// Package icon renders the menu-bar dot programmatically (no binary assets).
//
// The app normally shows text only ("<session>% · <weekly>%"). When
// status.claude.com reports anything other than "All Systems Operational" a red
// dot is placed in front of those numbers. The dot is a plain coloured (i.e.
// non-template) image so macOS keeps it red on both light and dark menu bars.
package icon

import (
	"bytes"
	"image"
	"image/color"
	"image/draw"
	"image/png"
	"sync"
)

// canvas is 44×44 (22pt @2x); systray force-sizes the image to 16pt, so the dot
// is drawn well inside the canvas to end up visually dot-sized next to text.
const canvas = 44

const (
	cx     = canvas / 2.0
	cy     = canvas / 2.0
	radius = 10.0
)

var colRed = color.NRGBA{R: 0xff, G: 0x3b, B: 0x30, A: 0xff} // macOS system red

var (
	redOnce   sync.Once
	redPNG    []byte
	blankOnce sync.Once
	blankPNG  []byte
)

// RedDotPNG returns the encoded red dot. The result is computed once and shared,
// so callers must not modify the returned slice.
func RedDotPNG() []byte {
	redOnce.Do(func() {
		img := image.NewNRGBA(image.Rect(0, 0, canvas, canvas))
		drawDisc(img, cx, cy, radius, colRed)
		var buf bytes.Buffer
		_ = png.Encode(&buf, img)
		redPNG = buf.Bytes()
	})
	return redPNG
}

// BlankPNG returns a fully transparent image the same size as the dot. It is the
// off phase of the blink: setting a same-sized invisible image keeps the icon slot
// occupied, so the percentages stay put instead of jumping left and back twice a
// second. Removing the icon outright is right only when the outage is over.
//
// The result is computed once and shared, so callers must not modify the returned
// slice.
func BlankPNG() []byte {
	blankOnce.Do(func() {
		img := image.NewNRGBA(image.Rect(0, 0, canvas, canvas))
		var buf bytes.Buffer
		_ = png.Encode(&buf, img)
		blankPNG = buf.Bytes()
	})
	return blankPNG
}

// drawDisc paints an anti-aliased filled circle by 4×4 supersampling each pixel.
func drawDisc(dst *image.NRGBA, centerX, centerY, rad float64, col color.NRGBA) {
	const ss = 4
	b := dst.Bounds()
	mask := image.NewAlpha(b)
	for py := b.Min.Y; py < b.Max.Y; py++ {
		for px := b.Min.X; px < b.Max.X; px++ {
			hits := 0
			for sy := 0; sy < ss; sy++ {
				for sx := 0; sx < ss; sx++ {
					x := float64(px) + (float64(sx)+0.5)/ss - centerX
					y := float64(py) + (float64(sy)+0.5)/ss - centerY
					if x*x+y*y <= rad*rad {
						hits++
					}
				}
			}
			if hits > 0 {
				mask.SetAlpha(px, py, color.Alpha{A: uint8(hits * 255 / (ss * ss))})
			}
		}
	}
	draw.DrawMask(dst, b, image.NewUniform(col), image.Point{}, mask, image.Point{}, draw.Over)
}
