// libzmq PUB: connects to the proxy front and sends fixed-size messages as
// fast as it can for `secs` seconds. Zero-copy on the libzmq side
// (zmq_msg_init_data over one static buffer) so the publisher is cheap.
#include <zmq.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
int main(int argc,char**argv){
  if(argc<4){fprintf(stderr,"pub ENDPOINT SIZE SECS [HWM]\n");return 1;}
  size_t size=strtoull(argv[2],0,10); double secs=atof(argv[3]);
  void*ctx=zmq_ctx_new(); void*s=zmq_socket(ctx,ZMQ_PUB);
  if(argc>4){int h=atoi(argv[4]);zmq_setsockopt(s,ZMQ_SNDHWM,&h,sizeof h);}
  if(!strncmp(argv[1],"bind:",5)) zmq_bind(s,argv[1]+5); else zmq_connect(s,argv[1]);
  char*buf=malloc(size); memset(buf,'x',size); buf[0]='A';
  // Wait for a subscription to propagate through the proxy.
  double t0=now(); while(now()-t0<1.5){ zmq_msg_t m; zmq_msg_init_data(&m,buf,size,NULL,NULL); zmq_msg_send(&m,s,ZMQ_DONTWAIT); zmq_msg_close(&m);} 
  unsigned long long sent=0; t0=now(); double end=t0+secs;
  while(1){
    for(int i=0;i<64;i++){ zmq_msg_t m; zmq_msg_init_data(&m,buf,size,NULL,NULL);
      if(zmq_msg_send(&m,s,0)>=0) sent++; else zmq_msg_close(&m);}
    if(now()>=end) break;
  }
  double dt=now()-t0;
  printf("pub offered=%llu msgs in %.1fs (PUB drops silently at HWM)\n",sent,dt);
  int linger=0; zmq_setsockopt(s,ZMQ_LINGER,&linger,sizeof linger);
  zmq_close(s); zmq_ctx_term(ctx); return 0;
}
