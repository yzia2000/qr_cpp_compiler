// The libzmq baseline: XSUB front, XPUB back, zmq_proxy between them.
#include <zmq.h>
#include <stdio.h>
#include <stdlib.h>
int main(int argc,char**argv){
  if(argc<3){fprintf(stderr,"zproxy FRONT BACK [HWM] [IO_THREADS]\n");return 1;}
  void*ctx=zmq_ctx_new();
  if(argc>4){zmq_ctx_set(ctx,ZMQ_IO_THREADS,atoi(argv[4]));}
  void*f=zmq_socket(ctx,ZMQ_XSUB),*b=zmq_socket(ctx,ZMQ_XPUB);
  if(argc>3){int h=atoi(argv[3]);zmq_setsockopt(f,ZMQ_RCVHWM,&h,sizeof h);zmq_setsockopt(b,ZMQ_SNDHWM,&h,sizeof h);}
  if(zmq_bind(f,argv[1])||zmq_bind(b,argv[2])){perror("bind");return 1;}
  fprintf(stderr,"[zproxy] up\n");
  zmq_proxy(f,b,NULL); return 0;
}
