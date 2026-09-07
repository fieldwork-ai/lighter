#include <CoreServices/CoreServices.h>
#include <sys/event.h>
#include <sys/time.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static double now(void) { struct timeval t; gettimeofday(&t,0); return t.tv_sec+t.tv_usec/1e6; }
static void callback(ConstFSEventStreamRef s,void *ctx,size_t n,void *paths,const FSEventStreamEventFlags *flags,const FSEventStreamEventId *ids) {
 (void)s;(void)ctx;(void)ids; char **p=paths;
 for(size_t i=0;i<n;i++) { const char *leaf=strrchr(p[i],'/'); leaf=leaf?leaf+1:p[i];
 if(!strcmp(leaf,"reply")||!strcmp(leaf,"request")||(flags[i]&0x2f)) fprintf(stderr,"EXTERNAL_FSEVENT %.6f flags=%x path=%s batch=%zu\n",now(),flags[i],p[i],n);
 }
}
static void *vnodes(void *root) {
 int kq=kqueue(); char path[4096]; struct kevent ev[2]; const char *names[]={"request","reply"};
 for(int i=0;i<2;i++) { snprintf(path,sizeof(path),"%s/%s",(char*)root,names[i]); int fd=open(path,O_RDONLY); if(fd<0)exit(4); EV_SET(&ev[i],fd,EVFILT_VNODE,EV_ADD|EV_CLEAR,NOTE_WRITE|NOTE_EXTEND|NOTE_ATTRIB|NOTE_DELETE|NOTE_RENAME,0,(void*)names[i]); }
 if(kevent(kq,ev,2,0,0,0)<0)exit(5);
 fprintf(stderr,"VNODE_READY %.6f\n",now());
 for(;;) { struct kevent out[2]; int n=kevent(kq,0,0,out,2,0); for(int i=0;i<n;i++) {char data[20]={0}; pread(out[i].ident,data,19,0); fprintf(stderr,"VNODE %.6f flags=%x file=%s contents=%s\n",now(),out[i].fflags,(char*)out[i].udata,data); } }
}
int main(int argc,char **argv) {
 if(argc!=2)return 2; setvbuf(stderr,0,_IONBF,0); pthread_t thread; pthread_create(&thread,0,vnodes,argv[1]);
 CFStringRef root=CFStringCreateWithCString(0,argv[1],kCFStringEncodingUTF8); CFArrayRef paths=CFArrayCreate(0,(const void**)&root,1,&kCFTypeArrayCallBacks);
 FSEventStreamRef stream=FSEventStreamCreate(0,callback,0,paths,kFSEventStreamEventIdSinceNow,.005,kFSEventStreamCreateFlagNoDefer|kFSEventStreamCreateFlagWatchRoot|kFSEventStreamCreateFlagFileEvents);
 FSEventStreamSetDispatchQueue(stream,dispatch_get_global_queue(QOS_CLASS_USER_INITIATED,0)); if(!FSEventStreamStart(stream))return 3;
 fprintf(stderr,"EXTERNAL_READY %.6f\n",now()); for(;;)pause();
}
