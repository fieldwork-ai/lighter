#!/usr/bin/env bash
# Milestone 22 gate: Kubernetes whose nodes are containers, beyond kind's own
# network plugin.
#
# kind with kindnet routes pod traffic and needs no overlay, so it passed long
# before anything else did. These are the two that need more of the guest's
# kernel, each as its users run it, with no flags to work around lighter:
#
#   k3d            k3s's flannel over VXLAN, kube-router's network policy
#                  (ipset, physdev, NFLOG), and Service traffic between pods
#                  on one node's bridge, which needs bridged netfilter in the
#                  node's own namespace (#106)
#   kind + Cilium  Cilium replacing kube-proxy: its start check
#                  (bpf_get_cgroup_classid in socket programs), BTF for its
#                  CO-RE programs, its VXLAN tunnel (#108)
#
# Each: Service DNS and HTTP from pods on both nodes, a deny-all policy that
# blocks and then lifts, and a container's own UDP through the guest still
# answering beside the cluster (bridged netfilter stays off in the guest's
# namespace).
#
# Needs kind, k3d, helm and kubectl on the Mac, and the network: the nodes
# pull their images.
# Not `set -e`: a cluster the kernel cannot run fails check after check, and
# each says so rather than the first ending the gate.
set -uo pipefail

if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	. "$HOME/.cargo/env"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

for tool in docker kind k3d helm kubectl; do
	command -v "$tool" >/dev/null 2>&1 || {
		echo "m22: skipped, needs $tool (brew install kind k3d helm kubectl)"
		exit 0
	}
done

# Pinned, so a pass is the same claim every time: the versions the issues
# were reported on.
KIND_NODE="kindest/node:v1.37.0@sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5"
K3S_IMAGE="rancher/k3s:v1.35.5-k3s1"
CILIUM_VERSION="1.20.2"
WEB_IMAGE="nginx:1.29-alpine"
PROBE_IMAGE="alpine:3.21"

KERNEL="${LIGHTER_GATE_KERNEL:-guest/out/Image}"
ROOTFS_DIR="$(mktemp -d -t lighter-rootfs)"
ROOTFS="$ROOTFS_DIR/rootfs.ext4"
cp -c guest/out/rootfs.ext4 "$ROOTFS" 2>/dev/null || cp guest/out/rootfs.ext4 "$ROOTFS"
PROFILE="${PROFILE:-debug}"
BIN="target/$PROFILE/examples/lighter-bench"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-120}"

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

RUN_DIR="$(mktemp -d -t lighter-m22)"
SOCKET="$RUN_DIR/docker.sock"
LOG="$RUN_DIR/machine.log"
VMM_PID=""
export DOCKER_HOST="unix://$SOCKET"
unset DOCKER_CONTEXT
export KUBECONFIG="$RUN_DIR/kubeconfig"

cleanup() {
	k3d cluster delete m22 >/dev/null 2>&1 || true
	kind delete cluster --name m22 >/dev/null 2>&1 || true
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -rf "$RUN_DIR" "$ROOTFS_DIR"
}
trap cleanup EXIT
trap 'exit 143' INT TERM

echo
echo "==> Booting a guest with room for two clusters' nodes"
"$BIN" \
	--kernel "$KERNEL" \
	--disk "$ROOTFS" \
	--disk "$RUN_DIR/data.img" --disk-size-gib 32 \
	--net --run-dir "$RUN_DIR" \
	--vsock "$SOCKET:2375" \
	--docker-ports "$SOCKET" \
	--no-tty --cpus 8 --memory-mib 12288 \
	--cmdline "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s)" \
	>"$LOG" 2>&1 &
VMM_PID=$!
waited=0
until grep -q "INIT dockerd=ready" "$LOG" 2>/dev/null; do
	kill -0 "$VMM_PID" 2>/dev/null || { fail "the VMM exited during boot"; tail -20 "$LOG"; exit 1; }
	[ "$waited" -lt "$BOOT_TIMEOUT" ] || { fail "dockerd did not come up within ${BOOT_TIMEOUT}s"; exit 1; }
	sleep 1
	waited=$((waited + 1))
done
pass "guest booted (${waited}s)"

# A pod on `node` runs `script` to completion; its output. Not `kubectl run
# -i`, whose attach loses the output of a pod that finishes before it joins.
probe() {
	local node="$1" script="$2" name="m22-probe-$RANDOM"
	kubectl run "$name" --restart=Never --image="$PROBE_IMAGE" \
		--overrides="{\"spec\":{\"nodeName\":\"$node\",\"tolerations\":[{\"operator\":\"Exists\"}]}}" \
		-- sh -c "$script" >/dev/null
	kubectl wait --for=jsonpath='{.status.phase}'=Succeeded "pod/$name" --timeout=120s >/dev/null 2>&1 || true
	kubectl logs "$name" 2>/dev/null || true
	kubectl delete pod "$name" --wait=false >/dev/null 2>&1 || true
}

# Service DNS and HTTP from a pod on each node, with the Service's one
# endpoint on the first: same-node and cross-node.
services() {
	local cluster="$1" first="$2" second="$3"
	kubectl create deployment web --image="$WEB_IMAGE" >/dev/null
	kubectl patch deployment web -p "{\"spec\":{\"template\":{\"spec\":{\"nodeName\":\"$first\",\"tolerations\":[{\"operator\":\"Exists\"}]}}}}" >/dev/null
	kubectl expose deployment web --port 80 >/dev/null
	kubectl rollout status deployment/web --timeout=180s >/dev/null || { fail "$cluster: the web deployment did not come up"; return; }
	for node in "$first" "$second"; do
		local out
		out="$(probe "$node" 'nslookup web.default.svc.cluster.local >/dev/null 2>&1 && echo DNS; wget -q -T5 -O /dev/null http://web 2>/dev/null && echo HTTP; nslookup -timeout=3 example.com >/dev/null 2>&1 && echo EXTERNAL')"
		[ "$(echo $out)" = "DNS HTTP EXTERNAL" ] \
			&& pass "$cluster: a pod on $node resolves and reaches the Service, and names outside" \
			|| fail "$cluster: a pod on $node: got [$(echo $out)], wanted DNS HTTP EXTERNAL"
	done
}

# A deny-all ingress policy blocks the Service from both nodes, and lifting
# it lets it through again.
policy() {
	local cluster="$1" first="$2" second="$3" verdicts=""
	kubectl apply -f - >/dev/null <<-EOF
		apiVersion: networking.k8s.io/v1
		kind: NetworkPolicy
		metadata: {name: deny-all}
		spec: {podSelector: {}, policyTypes: [Ingress]}
	EOF
	sleep 10
	for node in "$first" "$second"; do
		verdicts="$verdicts $(probe "$node" 'wget -q -T4 -O /dev/null http://web 2>/dev/null && echo reached || echo blocked')"
	done
	kubectl delete networkpolicy deny-all >/dev/null
	sleep 10
	for node in "$first" "$second"; do
		verdicts="$verdicts $(probe "$node" 'wget -q -T4 -O /dev/null http://web 2>/dev/null && echo reached || echo blocked')"
	done
	[ "$(echo $verdicts)" = "blocked blocked reached reached" ] \
		&& pass "$cluster: a deny-all policy blocks the Service from both nodes and lifts" \
		|| fail "$cluster: policy, then lifted: [$(echo $verdicts)], wanted blocked blocked reached reached"
}

guest_udp() {
	docker run --rm "$PROBE_IMAGE" nslookup -timeout=3 example.com 1.1.1.1 >/dev/null 2>&1 \
		&& pass "$1: a container's own UDP still reaches a resolver beside the cluster" \
		|| fail "$1: a container's UDP to 1.1.1.1 failed beside the cluster"
}

echo
echo "==> k3d, as created with no flags"
# `--wait` returns once the server answers, before flannel has set up its
# network: on a kernel without VXLAN the server then exits and restarts in a
# loop. A cluster is up when its nodes are Ready and the server has stayed.
k3d_up() {
	k3d cluster create m22 --agents 1 --image "$K3S_IMAGE" --wait --timeout 300s >"$RUN_DIR/k3d.log" 2>&1 \
		&& kubectl wait --for=condition=Ready nodes --all --timeout=120s >/dev/null 2>&1 \
		&& sleep 15 \
		&& ! grep -q "Shutdown request received" <<<"$(docker logs k3d-m22-server-0 2>&1)"
}
if k3d_up; then
	pass "k3d: a cluster of a server and an agent starts (flannel over VXLAN)"
	kubectl -n kube-system rollout status deployment/coredns --timeout=180s >/dev/null 2>&1 \
		|| fail "k3d: CoreDNS did not come up"
	services k3d k3d-m22-server-0 k3d-m22-agent-0
	# Read once: `grep -q` on a pipe closes it early, which pipefail reads
	# as the logs failing.
	server_log="$(docker logs k3d-m22-server-0 2>&1)"
	grep -q "Starting network policy controller" <<<"$server_log" \
		&& ! grep -qE "Aborting sync|ipset v[0-9.]+: Kernel error" <<<"$server_log" \
		&& pass "k3d: kube-router's network policy controller runs and loads its rules" \
		|| fail "k3d: kube-router: $(grep -m1 -E 'Aborting sync|ipset v' <<<"$server_log" || echo 'did not start')"
	policy k3d k3d-m22-server-0 k3d-m22-agent-0
	guest_udp k3d
else
	fail "k3d: the cluster did not start: $(grep -m1 -iE 'error|vxlan|shutdown' "$RUN_DIR/k3d.log" || tail -1 "$RUN_DIR/k3d.log")"
	docker logs k3d-m22-server-0 2>&1 | grep -m2 -E 'vxlan|Shutdown request' | sed 's/^/    /' || true
fi
k3d cluster delete m22 >/dev/null 2>&1 || true

echo
echo "==> kind with Cilium replacing kube-proxy"
cat >"$RUN_DIR/kind.yaml" <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes: [{role: control-plane}, {role: worker}]
networking: {disableDefaultCNI: true, kubeProxyMode: none}
EOF
if kind create cluster --name m22 --image "$KIND_NODE" --config "$RUN_DIR/kind.yaml" --kubeconfig "$KUBECONFIG" >"$RUN_DIR/kind.log" 2>&1; then
	helm repo add cilium https://helm.cilium.io >/dev/null 2>&1 || true
	helm repo update cilium >/dev/null 2>&1 || true
	if helm install cilium cilium/cilium --version "$CILIUM_VERSION" -n kube-system \
		--set kubeProxyReplacement=true --set k8sServiceHost=m22-control-plane --set k8sServicePort=6443 \
		>"$RUN_DIR/helm.log" 2>&1 \
		&& kubectl -n kube-system rollout status daemonset/cilium --timeout=400s >/dev/null 2>&1; then
		status="$(kubectl -n kube-system exec daemonset/cilium -c cilium-agent -- cilium-dbg status 2>/dev/null || true)"
		grep -qE '^KubeProxyReplacement:\s+True' <<<"$status" && grep -qE '^Cilium:\s+Ok' <<<"$status" \
			&& pass "kind + Cilium $CILIUM_VERSION: the agents run, kube-proxy replaced" \
			|| fail "kind + Cilium: status: $(echo "$status" | grep -E '^(Cilium|KubeProxyReplacement):' | tr '\n' ' ')"
		refusals="$(kubectl -n kube-system logs daemonset/cilium -c cilium-agent 2>/dev/null | grep -m1 -E 'requirements failed|no BTF found|operation not supported|creating socket destroyer' || true)"
		[ -z "$refusals" ] && pass "kind + Cilium: no kernel requirement refused" || fail "kind + Cilium: ${refusals:0:200}"
		kubectl -n kube-system rollout status deployment/coredns --timeout=180s >/dev/null 2>&1 \
			|| fail "kind + Cilium: CoreDNS did not come up (pods without a network?)"
		services "kind + Cilium" m22-worker m22-control-plane
		policy "kind + Cilium" m22-worker m22-control-plane
		guest_udp "kind + Cilium"
	else
		fail "kind + Cilium: the agents did not come up: $(kubectl -n kube-system logs daemonset/cilium -c cilium-agent 2>/dev/null | grep -m1 -E 'level=(error|fatal)|requirements failed' | cut -c1-200)"
	fi
else
	fail "kind: the cluster did not start: $(tail -1 "$RUN_DIR/kind.log")"
fi

echo
[ "$FAILED" = 0 ] && echo "m22: pass" || echo "m22: FAIL"
exit "$FAILED"
