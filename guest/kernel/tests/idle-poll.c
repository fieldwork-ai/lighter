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

/* The Python runner inserts the unmodified idle_poll function here. */
@IDLE_POLL@

static void idle_poll_adjust(u64 elapsed) { (void)elapsed; }
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
    return failures != 0;
}
