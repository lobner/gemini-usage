module geminiusage

go 1.25.11

require fyne.io/systray v1.12.1

require (
	github.com/godbus/dbus/v5 v5.1.0 // indirect
	golang.org/x/sys v0.15.0 // indirect
)

// Local fork of fyne.io/systray with a one-method fix to show_menu so the
// dropdown opens directly below the menu bar instead of over the icons.
// See third_party/systray/systray_darwin.m.
replace fyne.io/systray => ./third_party/systray
