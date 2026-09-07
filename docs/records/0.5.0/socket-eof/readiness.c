#include <sys/socket.h>
#include <pthread.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <errno.h>
#include <time.h>
#include <string.h>
#include <poll.h>
static ssize_t ready_recv(int fd,void *buf,size_t len,int flags) {
 for(;;){
  ssize_t n=recv(fd,buf,len,flags|MSG_DONTWAIT);
  if(n>=0)return n;
  if(errno==EINTR)continue;
  if(errno!=EAGAIN)return n;
  struct pollfd p={fd,POLLIN,0};
  int rc=poll(&p,1,200);
  if(rc==0){errno=EAGAIN;return -1;}
  if(rc<0 && errno!=EINTR)return -1;
 }
}
static int peer_failed;
static void *peer(void *arg) {
 pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE,0);int fd=*(int*)arg; char b[1024]={0};
 send(fd,b,284,0); ready_recv(fd,b,1,0);
 send(fd,b,23,0);sched_yield(); shutdown(fd,SHUT_WR);
 ssize_t n; while((n=ready_recv(fd,b,sizeof(b),0))>0){}
 if(n<0){int e=errno;ssize_t peek=ready_recv(fd,b,1,MSG_PEEK|MSG_DONTWAIT);printf("PEER_HUNG errno=%d peek=%zd\n",e,peek);fflush(stdout);peer_failed++;}
 return NULL;
}
int main(int argc,char **argv){
 pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE,0);int runs=argc>1?atoi(argv[1]):20000,failed=0;
 char *buf=malloc(1<<20);
 for(int i=0;i<runs;i++){
  int fds[2];socketpair(AF_UNIX,SOCK_STREAM,0,fds);if(i%2){int tmp=fds[0];fds[0]=fds[1];fds[1]=tmp;}
  int size=4<<20;
  for(int opt=0;opt<2;opt++)setsockopt(fds[0],SOL_SOCKET,opt?SO_SNDBUF:SO_RCVBUF,&size,sizeof(size));
  struct timeval tv={0,200000};setsockopt(fds[0],SOL_SOCKET,SO_RCVTIMEO,&tv,sizeof(tv));setsockopt(fds[1],SOL_SOCKET,SO_RCVTIMEO,&tv,sizeof(tv));
  pthread_t th;pthread_create(&th,NULL,peer,&fds[1]);
  ssize_t n=ready_recv(fds[0],buf,1<<20,0);send(fds[0],"x",1,0);
  while((n=ready_recv(fds[0],buf,1<<20,0))>0){}
  if(n<0){int e=errno; ssize_t peek=ready_recv(fds[0],buf,1,MSG_PEEK|MSG_DONTWAIT);printf("HUNG trial=%d errno=%d peek=%zd\n",i,e,peek);fflush(stdout);failed++;}
  shutdown(fds[0],SHUT_WR);pthread_join(th,NULL);close(fds[0]);close(fds[1]);
 }
 printf("runs=%d host_failures=%d peer_failures=%d\n",runs,failed,peer_failed);return failed||peer_failed?1:0;
}
