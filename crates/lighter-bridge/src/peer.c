// Who is on the other end of a Unix socket: the process's code signature,
// checked against a requirement, from the audit token the kernel recorded
// for the connection. The token, not the pid: a pid can be reused between
// the check and the use, a token names one process.
#include <CoreFoundation/CoreFoundation.h>
#include <Security/Security.h>
#include <bsm/libbsm.h>
#include <stdio.h>
#include <sys/socket.h>
#include <sys/un.h>

int lighter_peer_satisfies(int fd, const char *requirement, char *err, size_t errlen) {
	audit_token_t token;
	socklen_t len = sizeof token;
	if (getsockopt(fd, SOL_LOCAL, LOCAL_PEERTOKEN, &token, &len) != 0) {
		snprintf(err, errlen, "no peer token");
		return 0;
	}
	SecCodeRef code = NULL;
	CFDataRef audit = CFDataCreate(NULL, (const UInt8 *)&token, sizeof token);
	const void *keys[] = {kSecGuestAttributeAudit};
	const void *values[] = {audit};
	CFDictionaryRef attrs =
	    CFDictionaryCreate(NULL, keys, values, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
	OSStatus status = SecCodeCopyGuestWithAttributes(NULL, attrs, kSecCSDefaultFlags, &code);
	CFRelease(attrs);
	CFRelease(audit);
	if (status != errSecSuccess || !code) {
		snprintf(err, errlen, "cannot read the caller's signature (%d)", (int)status);
		return 0;
	}
	CFStringRef text = CFStringCreateWithCString(NULL, requirement, kCFStringEncodingUTF8);
	SecRequirementRef req = NULL;
	status = SecRequirementCreateWithString(text, kSecCSDefaultFlags, &req);
	CFRelease(text);
	if (status != errSecSuccess || !req) {
		CFRelease(code);
		snprintf(err, errlen, "bad requirement (%d)", (int)status);
		return 0;
	}
	status = SecCodeCheckValidity(code, kSecCSDefaultFlags, req);
	CFRelease(req);
	CFRelease(code);
	if (status != errSecSuccess) {
		snprintf(err, errlen, "the caller is not lighter (%d)", (int)status);
		return 0;
	}
	return 1;
}
