/* The sockmap reader's place in a TCP stream: the actual tcp_read_skb from
 * patch 0030 over a modelled receive queue. The mocks are the queue, the
 * skb and its TCP control block, and a front pull that can be made to fail;
 * this checks what the reader is handed, not the kernel's memory handling. */
#include <assert.h>
#include <errno.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

typedef uint8_t u8;
typedef uint32_t u32;
typedef int32_t s32;

#define TCP_LISTEN 10
#define TCP_ESTABLISHED 1
#define TCPHDR_FIN 0x01

struct tcp_skb_cb { u32 seq; u32 end_seq; u8 tcp_flags; };
struct sk_buff {
	struct sk_buff *next;
	const char *data;
	u32 len;
	struct tcp_skb_cb cb;
};
struct sk_buff_head { struct sk_buff *head; };
struct sock { int sk_state; struct sk_buff_head sk_receive_queue; };
struct tcp_sock { struct sock sk; u32 copied_seq; u32 read_skb_seq; };
typedef int (*skb_read_actor_t)(struct sock *sk, struct sk_buff *skb);

#define TCP_SKB_CB(skb) (&(skb)->cb)
static struct tcp_sock *tcp_sk(struct sock *sk) { return (struct tcp_sock *)sk; }
static bool before(u32 seq1, u32 seq2) { return (s32)(seq1 - seq2) < 0; }
static struct sk_buff *skb_peek(struct sk_buff_head *q) { return q->head; }
static void __skb_unlink(struct sk_buff *skb, struct sk_buff_head *q)
{
	assert(q->head == skb);
	q->head = skb->next;
}
static bool skb_set_owner_sk_safe(struct sk_buff *skb, struct sock *sk) { (void)skb; (void)sk; return true; }
static unsigned warnings;
static int warn_on_once(int condition) { if (condition) warnings++; return condition; }
#define WARN_ON_ONCE(x) warn_on_once(!!(x))
static bool pull_fails;
static void *pskb_pull(struct sk_buff *skb, u32 len)
{
	assert(len <= skb->len);
	if (pull_fails)
		return NULL;
	skb->data += len;
	skb->len -= len;
	return (void *)skb->data;
}

@READ_FUNCTION@

/* The reader: appends what it is handed to a stream, or refuses. */
static char stream[256];
static unsigned stream_len, handed, fins;
static int refuse_at;
static int reader(struct sock *sk, struct sk_buff *skb)
{
	(void)sk;
	handed++;
	if (refuse_at && (int)handed == refuse_at)
		return -EAGAIN;
	memcpy(stream + stream_len, skb->data, skb->len);
	stream_len += skb->len;
	if (skb->cb.tcp_flags & TCPHDR_FIN)
		fins++;
	return skb->len;
}

static struct tcp_sock tp;
static struct sk_buff skbs[8];
static unsigned nskbs;

static void reset(u32 copied_seq)
{
	memset(&tp, 0, sizeof(tp));
	tp.sk.sk_state = TCP_ESTABLISHED;
	tp.copied_seq = copied_seq;
	tp.read_skb_seq = copied_seq; /* as sk_psock_start_verdict does */
	memset(skbs, 0, sizeof(skbs));
	nskbs = 0;
	stream_len = handed = fins = 0;
	refuse_at = 0;
	pull_fails = false;
	warnings = 0;
}

/* Queue a segment carrying `data` at `seq`, in order, as TCP would. */
static void queue(u32 seq, const char *data, u8 flags)
{
	struct sk_buff *skb = &skbs[nskbs++], **tail = &tp.sk.sk_receive_queue.head;

	skb->data = data;
	skb->len = strlen(data);
	skb->cb.seq = seq;
	skb->cb.end_seq = seq + skb->len + !!(flags & TCPHDR_FIN);
	skb->cb.tcp_flags = flags;
	while (*tail)
		tail = &(*tail)->next;
	*tail = skb;
}

static unsigned queued(void)
{
	unsigned n = 0;
	for (struct sk_buff *s = tp.sk.sk_receive_queue.head; s; s = s->next)
		n++;
	return n;
}

static void expect(const char *want)
{
	assert(stream_len == strlen(want));
	assert(memcmp(stream, want, stream_len) == 0);
}

int main(void)
{
	/* In order: everything handed over, the place at the end. */
	reset(1001);
	queue(1001, "ABCDEFGHIJKLMNOP", 0);
	queue(1017, "QRSTUVWX", 0);
	assert(tcp_read_skb(&tp.sk, reader) == 24);
	expect("ABCDEFGHIJKLMNOPQRSTUVWX");
	assert(tp.read_skb_seq == 1025 && queued() == 0);

	/* The controlled case: a second segment starting eight bytes into the
	 * first, both queued, delivers twenty-four bytes, not thirty-two. */
	reset(1001);
	queue(1001, "ABCDEFGHIJKLMNOP", 0);
	queue(1009, "IJKLMNOPQRSTUVWX", 0);
	assert(tcp_read_skb(&tp.sk, reader) == 24);
	expect("ABCDEFGHIJKLMNOPQRSTUVWX");
	assert(tp.read_skb_seq == 1025 && skbs[1].cb.seq == 1017);

	/* The same overlap arriving after the first read: the place carries
	 * across calls. */
	reset(1001);
	queue(1001, "ABCDEFGHIJKLMNOP", 0);
	assert(tcp_read_skb(&tp.sk, reader) == 16);
	queue(1009, "IJKLMNOPQRSTUVWX", 0);
	assert(tcp_read_skb(&tp.sk, reader) == 8);
	expect("ABCDEFGHIJKLMNOPQRSTUVWX");

	/* Joined after a process read part of the head skb. */
	reset(1005);
	queue(1001, "ABCDEFGHIJKLMNOP", 0);
	assert(tcp_read_skb(&tp.sk, reader) == 12);
	expect("EFGHIJKLMNOP");

	/* A retransmission of delivered data carrying the FIN: the end of the
	 * stream is handed over empty, once, and the read stops there. */
	reset(1001);
	queue(1001, "ABCDEFGHIJKLMNOP", 0);
	assert(tcp_read_skb(&tp.sk, reader) == 16);
	queue(1001, "ABCDEFGHIJKLMNOP", TCPHDR_FIN);
	queue(1018, "NOT REACHED", 0);
	assert(tcp_read_skb(&tp.sk, reader) == 0);
	expect("ABCDEFGHIJKLMNOP");
	assert(fins == 1 && tp.read_skb_seq == 1018 && queued() == 1);

	/* Sequence numbers wrap. */
	reset(0xfffffff8u);
	queue(0xfffffff8u, "ABCDEFGHIJKLMNOP", 0);
	queue(0, "IJKLMNOPQRSTUVWX", 0);
	assert(tcp_read_skb(&tp.sk, reader) == 24);
	expect("ABCDEFGHIJKLMNOPQRSTUVWX");
	assert(tp.read_skb_seq == 16);

	/* No memory for the pull: the skb stays queued, untouched, and the
	 * next read delivers it. */
	reset(1001);
	queue(1001, "ABCDEFGHIJKLMNOP", 0);
	assert(tcp_read_skb(&tp.sk, reader) == 16);
	queue(1009, "IJKLMNOPQRSTUVWX", 0);
	pull_fails = true;
	assert(tcp_read_skb(&tp.sk, reader) == -ENOMEM);
	assert(queued() == 1 && skbs[1].len == 16 && tp.read_skb_seq == 1017);
	pull_fails = false;
	assert(tcp_read_skb(&tp.sk, reader) == 8);
	expect("ABCDEFGHIJKLMNOPQRSTUVWX");

	/* The reader refuses partway (a paused source): what it was handed is
	 * consumed, the rest waits, and the next read carries on in place. */
	reset(1001);
	queue(1001, "ABCDEFGH", 0);
	queue(1005, "EFGHIJKL", 0);
	queue(1009, "IJKLMNOP", 0);
	refuse_at = 2;
	assert(tcp_read_skb(&tp.sk, reader) == 8);
	assert(queued() == 1 && tp.read_skb_seq == 1013);
	refuse_at = 0;
	assert(tcp_read_skb(&tp.sk, reader) == 4);
	expect("ABCDEFGHMNOP");

	/* A listening socket has no stream. */
	reset(1001);
	tp.sk.sk_state = TCP_LISTEN;
	assert(tcp_read_skb(&tp.sk, reader) == -ENOTCONN);

	assert(warnings == 0);
	puts("tcp-read-skb: ok");
	return 0;
}
