#import <Foundation/Foundation.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static id activity;
__attribute__((constructor)) static void begin_activity(void) {
  @autoreleasepool {
    const char *mode = getenv("LIGHTER_TEST_ACTIVITY");
    NSProcessInfo *process = [NSProcessInfo processInfo];
    if (mode && strcmp(mode, "active") == 0) {
      activity = [[process beginActivityWithOptions:NSActivityUserInitiatedAllowingIdleSystemSleep
          reason:@"VM filesystem event diagnostic"] retain];
    }
    fprintf(stderr, "TEST_ACTIVITY pid=%d mode=%s token=%s\n", getpid(), mode ? mode : "unset", activity ? "held" : "none");
  }
}
