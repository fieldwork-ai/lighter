/* Model a receive worker that cannot deliver until the reader drops the
 * socket lock. This executes the helper extracted from the kernel patch. */
#include <stdbool.h>
#include <stdio.h>
#include <assert.h>
struct sock { int sk_shutdown; };
struct sk_psock { int unused; };
#define DEFINE_WAIT_FUNC(name, fn) int name = 0
#define RCV_SHUTDOWN 1
#define SOCKWQ_ASYNC_WAITDATA 1
#define TASK_INTERRUPTIBLE 1
#define sk_sleep(sk) (sk)
static bool locked, data, waiting, async_wait, blocked_worker;
static unsigned waits;
static void add_wait_queue(struct sock *sk, int *w) { (void)sk; (void)w; waiting = true; }
static void remove_wait_queue(struct sock *sk, int *w) { (void)sk; (void)w; waiting = false; }
static void sk_set_bit(int bit, struct sock *sk) { (void)bit; (void)sk; async_wait = true; }
static void sk_clear_bit(int bit, struct sock *sk) { (void)bit; (void)sk; async_wait = false; }
static bool vsock_has_data(struct sock *sk, struct sk_psock *p) { (void)sk; (void)p; assert(locked); return data; }
static void release_sock(struct sock *sk) { (void)sk; assert(locked); locked = false; }
static void lock_sock(struct sock *sk) { (void)sk; assert(!locked); locked = true; }
static void wait_woken(int *w, int state, long timeout)
{
    (void)w; (void)state; (void)timeout;
    assert(waiting && async_wait);
    waits++;
    blocked_worker = locked;
    if (!locked) data = true;
}
@WAIT_FUNCTION@
int main(void)
{
    struct sock sk = {0};
    struct sk_psock psock = {0};
    locked = true;
    bool ready = vsock_msg_wait_data(&sk, &psock, 100);
    assert(ready && !blocked_worker && waits == 1);
    assert(locked && !waiting && !async_wait);
    waits = 0;
    assert(vsock_msg_wait_data(&sk, &psock, 100));
    assert(!waits && locked && !waiting && !async_wait);
    data = false;
    assert(!vsock_msg_wait_data(&sk, &psock, 0));
    assert(!waits && locked && !waiting && !async_wait);
    sk.sk_shutdown = RCV_SHUTDOWN;
    assert(vsock_msg_wait_data(&sk, &psock, 100));
    assert(!waits && locked && !waiting && !async_wait);
    puts("PASS: blocking receive releases socket lock; ready/nonblocking/shutdown preserve it");
}
