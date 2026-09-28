#!/usr/bin/env python3
"""The guest's nat and mangle rules against the agent's constants.

The inbound mark, the reply mark and the resolver's port are written twice,
once in the agent's Rust and once in init's nft ruleset, and nothing else
ties them together: a mismatch would not fail anything at build time, it
would send published DNS to the resolver or a published server's replies out
of the guest.
"""
import pathlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
INIT = (ROOT / 'guest/rootfs/init').read_text()
MAIN = (ROOT / 'guest/agent/src/main.rs').read_text()
INBOUND = (ROOT / 'guest/agent/src/udp_inbound.rs').read_text()


def rust_const(source, name):
    match = re.search(rf'const {name}: u\d+ = ([0-9a-fx_]+);', source)
    assert match, name
    return int(match.group(1).replace('_', ''), 0)


class InboundRules(unittest.TestCase):
    def setUp(self):
        self.mark = rust_const(INBOUND, 'SK_MARK_INBOUND')
        self.port = rust_const(MAIN, 'DNS_LISTEN')

    def test_the_mark_is_not_tproxys(self):
        self.assertNotEqual(self.mark, 0x1)

    def test_both_dns_rules_send_to_the_resolvers_port(self):
        rules = re.findall(r'udp dport 53 .*dnat ip to 192\.168\.127\.2:(\d+)', INIT)
        self.assertEqual(len(rules), 2, 'one in prerouting, one in output')
        self.assertTrue(all(int(p) == self.port for p in rules))

    def test_the_output_rule_leaves_inbound_flows_alone(self):
        self.assertIn(f'meta mark != {self.mark:#x} dnat', INIT)

    def test_the_inbound_mark_reaches_the_connection(self):
        self.assertIn(f'meta mark {self.mark:#x} ct mark set meta mark', INIT)

    def test_replies_route_by_a_mark_of_their_own(self):
        reply = re.search(rf'ct mark {self.mark:#x} meta mark set (0x[0-9a-f]+)', INIT)
        self.assertTrue(reply, 'the reply rule')
        route = int(reply.group(1), 16)
        self.assertNotEqual(route, self.mark, 'a route on the socket mark sends its own packets to lo')
        self.assertIn(f'ip rule add fwmark {route:#x} lookup 100', INIT)
        self.assertIn(f'ip -6 rule add fwmark {route:#x} lookup 100', INIT)


if __name__ == '__main__':
    unittest.main()
