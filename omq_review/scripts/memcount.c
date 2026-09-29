// LD_PRELOAD shim: tallies bytes moved by memcpy/memmove calls >= 4 KiB (payload-sized
// copies; small header/bookkeeping copies are ignored) and by realloc calls that moved the
// block. If MEMCOUNT_OUT is set, a helper thread rewrites the running totals into that file
// every 50 ms, so a caller can diff two snapshots (copyprobe.sh). Build:
//   gcc -O2 -shared -fPIC -o libmemcount.so memcount.c -ldl -lpthread
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <malloc.h>
#include <pthread.h>
#include <fcntl.h>
#include <stdlib.h>
#include <time.h>

static void *(*real_memcpy)(void *, const void *, size_t);
static void *(*real_memmove)(void *, const void *, size_t);
static _Atomic unsigned long long cp_bytes, cp_calls, mv_bytes, mv_calls, rl_bytes, rl_calls;
static void *(*real_realloc)(void *, size_t);
static void *(*real_calloc)(size_t, size_t);
static _Atomic unsigned long long ca_bytes, ca_calls;
#define THRESH 4096

// calloc: dlsym itself may call calloc, so serve those early calls from a static arena.
static char early_arena[4096];
static size_t early_used;
void *calloc(size_t n, size_t sz) {
  if (__builtin_expect(!real_calloc, 0)) {
    static __thread int in_dlsym;
    if (in_dlsym) {
      size_t need = (n * sz + 15) & ~(size_t)15;
      if (early_used + need > sizeof early_arena) return NULL;
      void *p = early_arena + early_used;
      early_used += need;
      return p;  // static storage is already zeroed
    }
    in_dlsym = 1;
    real_calloc = dlsym(RTLD_NEXT, "calloc");
    in_dlsym = 0;
  }
  size_t total = n * sz;
  if (total >= THRESH) { atomic_fetch_add_explicit(&ca_bytes, total, memory_order_relaxed);
                         atomic_fetch_add_explicit(&ca_calls, 1, memory_order_relaxed); }
  return real_calloc(n, sz);
}

static void *slow_copy(void *d, const void *s, size_t n) {
  unsigned char *dd = d; const unsigned char *ss = s;
  if (dd < ss) { for (size_t i = 0; i < n; i++) dd[i] = ss[i]; }
  else { for (size_t i = n; i > 0; i--) dd[i - 1] = ss[i - 1]; }
  return d;
}

void *memcpy(void *d, const void *s, size_t n) {
  if (__builtin_expect(!real_memcpy, 0)) {
    void *p = dlsym(RTLD_NEXT, "memcpy");
    if (!p) return slow_copy(d, s, n);
    real_memcpy = p;
  }
  if (n >= THRESH) { atomic_fetch_add_explicit(&cp_bytes, n, memory_order_relaxed);
                     atomic_fetch_add_explicit(&cp_calls, 1, memory_order_relaxed); }
  return real_memcpy(d, s, n);
}

void *memmove(void *d, const void *s, size_t n) {
  if (__builtin_expect(!real_memmove, 0)) {
    void *p = dlsym(RTLD_NEXT, "memmove");
    if (!p) return slow_copy(d, s, n);
    real_memmove = p;
  }
  if (n >= THRESH) { atomic_fetch_add_explicit(&mv_bytes, n, memory_order_relaxed);
                     atomic_fetch_add_explicit(&mv_calls, 1, memory_order_relaxed); }
  return real_memmove(d, s, n);
}

// realloc that moved the block: count the preserved prefix as copied (glibc copies it
// internally with its own memcpy, which the memcpy hook above cannot see). Large mmapped
// blocks may be moved with mremap (no copy), so this is an upper bound for those.
void *realloc(void *p, size_t n) {
  if (__builtin_expect(!real_realloc, 0)) real_realloc = dlsym(RTLD_NEXT, "realloc");
  size_t old = p ? malloc_usable_size(p) : 0;
  void *r = real_realloc(p, n);
  if (r && p && r != p) {
    size_t kept = old < n ? old : n;
    if (kept >= THRESH) { atomic_fetch_add_explicit(&rl_bytes, kept, memory_order_relaxed);
                          atomic_fetch_add_explicit(&rl_calls, 1, memory_order_relaxed); }
  }
  return r;
}

// No signals (they EINTR libzmq's zmq_proxy): a helper thread rewrites the totals into
// $MEMCOUNT_OUT every 50 ms; the driver reads that file before and after the run.
static void *writer(void *arg) {
  const char *path = arg;
  for (;;) {
    char buf[256];
    int n = snprintf(buf, sizeof buf, "MEMCOUNT memcpy_bytes=%llu memcpy_calls=%llu memmove_bytes=%llu memmove_calls=%llu realloc_moved_bytes=%llu realloc_moves=%llu calloc_bytes=%llu calloc_calls=%llu\n",
                     (unsigned long long)cp_bytes, (unsigned long long)cp_calls,
                     (unsigned long long)mv_bytes, (unsigned long long)mv_calls,
                     (unsigned long long)rl_bytes, (unsigned long long)rl_calls,
                     (unsigned long long)ca_bytes, (unsigned long long)ca_calls);
    char tmp[512]; snprintf(tmp, sizeof tmp, "%s.tmp", path);
    int fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd >= 0) { ssize_t w = write(fd, buf, (size_t)n); (void)w; close(fd); rename(tmp, path); }
    struct timespec ts = {0, 50 * 1000 * 1000};
    nanosleep(&ts, NULL);
  }
  return NULL;
}

__attribute__((constructor)) static void init(void) {
  const char *out = getenv("MEMCOUNT_OUT");
  if (out) { pthread_t t; pthread_create(&t, NULL, writer, (void *)out); pthread_detach(t); }
}
