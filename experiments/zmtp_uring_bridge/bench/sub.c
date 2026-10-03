// libzmq SUB: connects to the proxy back, subscribes to everything, and
// reports received throughput over a steady window (skipping warm-up).
#include <zmq.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
int main(int argc,char**argv){
  if(argc<5){fprintf(stderr,"sub ENDPOINT SIZE WARMUP_SECS MEASURE_SECS\n");return 1;}
  size_t size=strtoull(argv[2],0,10); double warm=atof(argv[3]), meas=atof(argv[4]);
  void*ctx=zmq_ctx_new(); void*s=zmq_socket(ctx,ZMQ_SUB);
  zmq_setsockopt(s,ZMQ_SUBSCRIBE,"",0); zmq_connect(s,argv[1]);
  int to=5000; zmq_setsockopt(s,ZMQ_RCVTIMEO,&to,sizeof to);
  zmq_msg_t m; zmq_msg_init(&m);
  if(zmq_msg_recv(&m,s,0)<0){printf("sub: no data\n");return 2;}
  double first=now(), start=first+warm, end=start+meas; unsigned long long n=0,bytes=0,bad=0; int counting=0; double t;
  while((t=now())<end){
    if(zmq_msg_recv(&m,s,0)<0){printf("sub: timeout\n");break;}
    if(zmq_msg_size(&m)!=size) bad++;
    if(!counting && t>=start){counting=1;n=0;bytes=0;}
    n++; bytes+=zmq_msg_size(&m);
  }
  double dt=now()-start;
  printf("RESULT size=%zu msgs=%llu rate=%.0f msg/s throughput=%.1f MB/s (%.2f Gbit/s) badsize=%llu\n",size,n,n/dt,bytes/dt/1e6,bytes*8/dt/1e9,bad);
  zmq_msg_close(&m); int linger=0; zmq_setsockopt(s,ZMQ_LINGER,&linger,sizeof linger); zmq_close(s); zmq_ctx_term(ctx); return 0;
}
