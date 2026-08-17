package login

/*
// No -fobjc-arc: this file allocates no Objective-C objects, and asking for ARC
// pulls in a second -lobjc alongside systray's, which the linker warns about.
#cgo LDFLAGS: -framework Foundation -framework ServiceManagement

#include <stdlib.h>

int loginStatus(void);
int loginRegister(char **errOut);
int loginUnregister(char **errOut);
*/
import "C"

import (
	"errors"
	"unsafe"
)

// SMAppServiceStatus values we care about.
const (
	statusUnsupported      = -1 // macOS 12 or earlier: no SMAppService
	statusNotRegistered    = 0
	statusEnabled          = 1
	statusRequiresApproval = 2
)

// Supported reports whether this macOS can register the app as a login item.
func Supported() bool {
	return int(C.loginStatus()) != statusUnsupported
}

// Enabled reports whether the app currently opens at login.
//
// It combines what the system says with what we recorded, because neither is
// sufficient alone: SMAppService reports "enabled" for a bundle that has never
// been registered, so a positive status is only believable once we have
// registered it; and the user can switch the item off in System Settings behind
// our back, which the status does reflect.
func Enabled() bool {
	switch int(C.loginStatus()) {
	case statusEnabled:
		return Answer()
	default: // notRegistered, requiresApproval, unsupported
		return false
	}
}

// Enable registers the app to open at login. macOS remembers the bundle by the
// path it is registered from, so register the copy that is meant to stay put.
func Enable() error {
	return change(func(p **C.char) C.int { return C.loginRegister(p) })
}

// Disable unregisters the login item.
func Disable() error {
	return change(func(p **C.char) C.int { return C.loginUnregister(p) })
}

// NeedsApproval reports whether macOS is holding the login item back until the
// user switches it on themselves, which happens when they have previously
// switched it off in System Settings.
func NeedsApproval() bool {
	return int(C.loginStatus()) == statusRequiresApproval
}

func change(call func(**C.char) C.int) error {
	var cerr *C.char
	rc := int(call(&cerr))
	msg := ""
	if cerr != nil {
		msg = C.GoString(cerr)
		C.free(unsafe.Pointer(cerr))
	}
	if rc == 0 {
		return nil
	}
	if msg == "" {
		msg = "the login item could not be changed"
	}
	return errors.New(msg)
}

// Reconcile makes the system match the recorded intent, and reports whether the
// app is registered afterwards. Call it at startup when Answer is true.
//
// It exists because the system cannot be asked "is this bundle registered?":
// SMAppService answers "enabled" for one that never was. So when our record says
// we registered and the system gives that ambiguous answer, register again — a
// no-op if it was already true, and self-healing when the record is stale or the
// app has been moved since (macOS remembers the path it was registered from).
//
// A status of notRegistered or requiresApproval means someone switched the item
// off outside the app, most likely in System Settings. That is a decision to
// respect, not to undo, so it is left alone and reported as not registered.
func Reconcile() bool {
	if !Answer() {
		return false
	}
	if int(C.loginStatus()) != statusEnabled {
		return false
	}
	return Enable() == nil
}
