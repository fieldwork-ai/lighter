/* Deterministic wakeup interleavings for the actual function in patch 0011.
 * The mocks model NEED_RESCHED and the IPI suppression while polling; this
 * checks control flow, not the kernel helpers' atomic memory ordering. */
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>

typedef uint32_t u32;
typedef uint64_t u64;
#define __cpuidle
#define __this_cpu_read(value) (value)
#define __this_cpu_write(variable, value) ((variable) = (value))

enum scenario { DISABLED, TIMEOUT, ENTRY, LOOP, DEADLINE, BEFORE_CLEAR, AFTER_CLEAR, TIMER };
static enum scenario scenario;
static u32 idle_poll_limit_ns;
static bool pending, polling, irq_enabled, ipi;
static unsigned clocks, relaxations;
static unsigned idle_poll_ns = 200000;
static bool idle_poll_halt_next;
static bool timer_reprogram, slept_before_reprogram;
static unsigned sleeps;
static unsigned idle_poll_grow_start_ns = 50000;
static unsigned idle_poll_local_ns = 200000;
static u64 next_timer_mock, now_mock, traffic_mock;
static u64 idle_poll_block_ns;
static bool traffic_recent_mock;
static bool idle_traffic_recent(void) { return traffic_recent_mock; }
enum { JUDGED_TIMER, JUDGED_LONG, JUDGED_TRAFFIC, JUDGED_LOCAL, JUDGED_KINDS };
static unsigned long idle_poll_judged[JUDGED_KINDS];
#define __this_cpu_inc(counter) ((counter)++)
static bool idle_poll_block_timer, idle_poll_block_pending;
#define min(a, b) ((a) < (b) ? (a) : (b))
#define max(a, b) ((a) > (b) ? (a) : (b))
static u64 idle_traffic(void) { return traffic_mock; }
#define min_t(type, a, b) ((type)(a) < (type)(b) ? (type)(a) : (type)(b))
#define instrumentation_begin()
#define instrumentation_end()
static u64 ktime_get_mono_fast_ns(void) { return now_mock; }
static u64 idle_next_timer_ns(void) { return next_timer_mock; }

static void wake(void)
{
	pending = true;
	if (!polling)
		ipi = true;
}

static u64 local_clock_noinstr(void)
{
	if (++clocks == 2 && scenario == DEADLINE)
		wake();
	return clocks * 1000;
}

static void raw_local_irq_enable(void) { irq_enabled = true; }
static void raw_local_irq_disable(void) { irq_enabled = false; }
static bool current_set_polling_and_test(void)
{
	polling = true;
	if (scenario == ENTRY)
		wake();
	return pending;
}
static bool need_resched(void) { return pending; }
static void cpu_relax(void)
{
    if (++relaxations == 1 && scenario == LOOP)
        wake();
    if (relaxations == 1 && scenario == TIMER && irq_enabled)
        timer_reprogram = true;
}
static void current_clr_polling(void)
{
	if (scenario == BEFORE_CLEAR)
		wake();
	polling = false;
}
static bool current_clr_polling_and_test(void)
{
	current_clr_polling();
	bool result = pending;
	if (scenario == AFTER_CLEAR)
		wake();
	return result;
}

/* The Python runner inserts the unmodified functions here. */
@IDLE_SLACK@
@IDLE_TIMER_DUE@
@IDLE_ADJUST@
@IDLE_RECORD@
@IDLE_POLL@

static void cpu_do_idle(void)
{
    sleeps++;
    slept_before_reprogram |= timer_reprogram;
}

@ARCH_IDLE@

int main(void)
{
	static const char *names[] = {
		"disabled", "quiet timeout", "pending at entry", "wake in loop",
		"wake during deadline check", "wake before clear", "wake after clear"
	};
	int failures = 0;
	for (scenario = DISABLED; scenario <= AFTER_CLEAR; scenario++) {
		idle_poll_limit_ns = scenario == DISABLED ? 0 : 500;
		pending = polling = irq_enabled = ipi = false;
		clocks = relaxations = 0;
		u64 start;
		bool woke = idle_poll(&start);
		bool expected = scenario >= ENTRY && scenario <= BEFORE_CLEAR;
		if (woke != expected || polling || irq_enabled ||
		    (pending && !woke && !ipi)) {
			fprintf(stderr, "FAIL: %s (woke=%d pending=%d polling=%d irq=%d ipi=%d)\n",
				names[scenario], woke, pending, polling, irq_enabled, ipi);
			failures++;
		} else {
			printf("PASS: %s\n", names[scenario]);
		}
	}
    /* An interrupt can queue a timer without requesting a reschedule.
     * kernel/sched/idle.c must get another turn to update the stopped tick
     * before the architecture sleeps. Merely testing NEED_RESCHED misses it. */
    scenario = TIMER;
    next_timer_mock = 10000000;
    now_mock = 1000000;
    idle_poll_limit_ns = 500;
    idle_poll_halt_next = false;
    pending = polling = irq_enabled = ipi = false;
    timer_reprogram = slept_before_reprogram = false;
    clocks = relaxations = sleeps = 0;
    arch_cpu_idle();
    if (sleeps || !timer_reprogram || slept_before_reprogram || polling || irq_enabled) {
        fprintf(stderr, "FAIL: timer queued while polling reached WFI before scheduler reprogramming\n");
        failures++;
    } else {
        /* Model the next generic idle iteration reprogramming the tick. */
        timer_reprogram = false;
        arch_cpu_idle();
        if (sleeps != 1 || slept_before_reprogram || irq_enabled) {
            fprintf(stderr, "FAIL: idle did not sleep after scheduler reprogramming\n");
            failures++;
        } else {
            printf("PASS: timer queued while polling returns through scheduler before WFI\n");
        }
    }
    /* The window grows only on a wakeup from outside: a block that ends
     * when the programmed timer was due says nothing about the next event
     * and was known in advance, so it halves the window instead. Past the
     * resting size it grows only on a wakeup that accelerator traffic
     * brought; one from inside the guest earns the resting window at most,
     * and brings a larger one back down to it. */
    struct { const char *name; unsigned cap; u64 block, next, now; bool traffic; u32 before, after; } rules[] = {
        { "outside wakeup within the cap doubles", 200000, 100000, 10000000, 1000000, false, 60000, 120000 },
        { "outside wakeup from nothing starts at the floor", 200000, 100000, 10000000, 1000000, false, 0, 50000 },
        { "outside wakeup past the cap halves", 200000, 300000, 10000000, 1000000, false, 100000, 50000 },
        { "timer wakeup within the cap halves", 200000, 100000, 1000000, 990000, false, 100000, 50000 },
        { "timer wakeup just late still halves", 200000, 100000, 1000000, 1020000, false, 100000, 50000 },
        { "wakeup well before the timer doubles", 200000, 100000, 1000000, 900000, false, 100000, 200000 },
        { "the resting cap bounds the growth", 200000, 100000, 10000000, 1000000, false, 150000, 200000 },
        { "a raised cap: traffic grows past the resting size", 5000000, 100000, 10000000, 1000000, true, 200000, 400000 },
        { "a raised cap: traffic reaches the cap", 5000000, 100000, 10000000, 1000000, true, 4000000, 5000000 },
        { "a raised cap: traffic grows whatever woke the CPU", 5000000, 100000, 1000000, 995000, true, 1000000, 2000000 },
        { "a raised cap: traffic grows however long the block", 5000000, 9000000, 100000000, 1000000, true, 2500000, 5000000 },
        { "a raised cap: a local wakeup stops at the resting size", 5000000, 100000, 10000000, 1000000, false, 150000, 200000 },
        { "a raised cap: a local wakeup halves a larger window", 5000000, 100000, 10000000, 1000000, false, 1600000, 800000 },
        { "a raised cap: a local wakeup never halves below the resting size", 5000000, 100000, 10000000, 1000000, false, 300000, 200000 },
        { "a raised cap: the timer halves a larger window to the resting size", 5000000, 100000, 1000000, 995000, false, 300000, 200000 },
        { "a raised cap: the timer halves at the resting size", 5000000, 100000, 1000000, 995000, false, 200000, 100000 },
        { "a raised cap: a block past it halves without traffic", 5000000, 9000000, 100000000, 1000000, false, 5000000, 2500000 },
    };
    for (unsigned i = 0; i < sizeof(rules) / sizeof(rules[0]); i++) {
        idle_poll_ns = rules[i].cap;
        idle_poll_limit_ns = rules[i].before;
        next_timer_mock = rules[i].next;
        now_mock = rules[i].now;
        idle_poll_adjust(rules[i].block, idle_timer_due(next_timer_mock), rules[i].traffic);
        if (idle_poll_limit_ns != rules[i].after) {
            fprintf(stderr, "FAIL: %s (limit %u, expected %u)\n",
                rules[i].name, idle_poll_limit_ns, rules[i].after);
            failures++;
        } else {
            printf("PASS: %s\n", rules[i].name);
        }
    }
    /* A block is judged at the next idle entry, when the CPU has run what
     * it woke for: a reply's bytes are counted by the worker the interrupt
     * wakes, after the wakeup itself. */
    scenario = ENTRY;
    idle_poll_ns = 5000000;
    idle_poll_limit_ns = 200000;
    idle_poll_halt_next = idle_poll_block_pending = false;
    next_timer_mock = 10000000; now_mock = 1000000; traffic_recent_mock = false;
    pending = polling = irq_enabled = ipi = false; clocks = relaxations = 0;
    arch_cpu_idle();
    if (!idle_poll_block_pending || idle_poll_limit_ns != 200000) {
        fprintf(stderr, "FAIL: the block was judged on waking (pending=%d limit=%u)\n",
            idle_poll_block_pending, idle_poll_limit_ns);
        failures++;
    } else {
        traffic_recent_mock = true;
        pending = polling = irq_enabled = ipi = false; clocks = relaxations = 0;
        arch_cpu_idle();
        if (idle_poll_limit_ns != 400000) {
            fprintf(stderr, "FAIL: a reply counted after waking did not earn the window (limit=%u)\n",
                idle_poll_limit_ns);
            failures++;
        } else {
            printf("PASS: a reply counted after waking earns the window at the next idle entry\n");
        }
    }
    return failures != 0;
}
