#!/usr/bin/env python3
"""Every long-lived agent init starts is supervised.

Each agent is a whole feature: the Docker socket, the Mac's control channel,
containers' TCP and UDP, published ports, DNS. In 0.10.3 the outbound proxy
panicked when the guest ran short of threads, nothing restarted it, and every
container's TCP was refused until the machine restarted. init now starts each
one through `supervise`, which restarts it; an agent started bare would bring
that back unnoticed.
"""
import pathlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
INIT = (ROOT / 'guest/rootfs/init').read_text()


class AgentSupervision(unittest.TestCase):
    def test_no_agent_is_started_bare(self):
        bare = [line.strip() for line in INIT.splitlines()
                if re.match(r'\s*"\$AGENT"\s', line) and 'supervise' not in line]
        # The one bare call is supervise's own.
        self.assertEqual(bare, ['"$AGENT" "$@"'], 'an agent started outside supervise')

    def test_every_service_is_supervised(self):
        supervised = re.findall(r'^\s*supervise (--[a-z-]+)', INIT, re.M)
        for mode in ['--port', '--tcp-proxy', '--udp-proxy', '--inbound', '--udp-inbound', '--dns']:
            self.assertIn(mode, supervised)
        self.assertEqual(supervised.count('--port'), 2, 'the Docker socket and control')

    def test_the_supervisor_survives_the_agents_exit(self):
        # init is `set -e`: without this, the subshell ends with its agent.
        body = INIT[INIT.index('supervise() {'):]
        body = body[:body.index('\n}\n')]
        self.assertIn('set +e', body)
        self.assertLess(body.index('set +e'), body.index('"$AGENT" "$@"'))

    def test_the_limits_are_raised_before_anything_starts(self):
        limits = INIT.index('kernel.threads-max=4194304 kernel.pid_max=4194304')
        self.assertLess(limits, INIT.index('# --- containerd'))
        self.assertLess(limits, INIT.index('supervise --port 2375'))


if __name__ == '__main__':
    unittest.main()
