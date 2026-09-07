#include <CoreServices/CoreServices.h>
#include <sys/time.h>
#include <stdio.h>
#include <string.h>
static double now(void) {struct timeval t;gettimeofday(&t,0);return t.tv_sec+t.tv_usec/1e6;}
static FSEventStreamCallback original;
static void cb(ConstFSEventStreamRef s,void *ctx,size_t n,void *paths,const FSEventStreamEventFlags *flags,const FSEventStreamEventId *ids) {
 char **p=paths; for(size_t i=0;i<n;i++){const char *leaf=strrchr(p[i],'/');leaf=leaf?leaf+1:p[i];if(!strcmp(leaf,"reply")||!strcmp(leaf,"request")||(flags[i]&0x2f))fprintf(stderr,"VMM_FSEVENT %.6f flags=%x path=%s batch=%zu\n",now(),flags[i],p[i],n);}
 double t=now();original(s,ctx,n,paths,flags,ids);double elapsed=now()-t;if(elapsed>.1)fprintf(stderr,"VMM_CALLBACK_SLOW %.6f seconds=%.6f count=%zu\n",now(),elapsed,n);
}
static FSEventStreamRef create(CFAllocatorRef a,FSEventStreamCallback c,FSEventStreamContext *ctx,CFArrayRef paths,FSEventStreamEventId since,CFTimeInterval latency,FSEventStreamCreateFlags flags) {
 original=c;fprintf(stderr,"VMM_STREAM %.6f flags=%x latency=%f\n",now(),flags,latency);return FSEventStreamCreate(a,cb,ctx,paths,since,latency,flags);
}
__attribute__((used)) static struct {const void *replacement;const void *replacee;} interpose __attribute__((section("__DATA,__interpose")))={(const void*)create,(const void*)FSEventStreamCreate};
