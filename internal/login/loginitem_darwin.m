#import <Foundation/Foundation.h>
#import <ServiceManagement/ServiceManagement.h>

#include <string.h>

// The SMAppService login-item API arrived in macOS 13; on 11 and 12 every entry
// point here reports statusUnsupported and registering fails, so the app simply
// never offers to launch at login.
#define statusUnsupported (-1)

int loginStatus(void) {
  if (@available(macOS 13.0, *)) {
    return (int)[SMAppService mainAppService].status;
  }
  return statusUnsupported;
}

// change registers or unregisters the running bundle as a login item. It returns
// 0 on success, or a non-zero code with a description in *errOut, which the
// caller owns and must free.
static int change(BOOL enable, char **errOut) {
  if (@available(macOS 13.0, *)) {
    NSError *err = nil;
    SMAppService *svc = [SMAppService mainAppService];
    BOOL ok = enable ? [svc registerAndReturnError:&err]
                     : [svc unregisterAndReturnError:&err];
    if (ok) {
      return 0;
    }
    if (errOut != NULL) {
      const char *desc = err.localizedDescription.UTF8String;
      *errOut = strdup(desc != NULL ? desc : "unknown error");
    }
    return err.code != 0 ? (int)err.code : statusUnsupported;
  }
  if (errOut != NULL) {
    *errOut = strdup("requires macOS 13 or later");
  }
  return statusUnsupported;
}

int loginRegister(char **errOut) { return change(YES, errOut); }

int loginUnregister(char **errOut) { return change(NO, errOut); }
