# Kubernetes with kind

Lighter 0.5.0 runs local Kubernetes clusters through kind. kind creates the
Kubernetes nodes as Docker containers inside Lighter's VM; `kubectl` and Helm
connect from macOS. Cluster creation and upgrades remain ordinary kind commands.

## Start a cluster

Install kind and kubectl, start Lighter and select its Docker context:

```sh
brew install kind kubectl
lighter start
docker context use lighter
kind version
```

The qualification uses **kind v0.33.0** and native arm64 node images. Pin the
node image to select the Kubernetes version explicitly:

```sh
KIND_EXPERIMENTAL_PROVIDER=docker kind create cluster --name dev \
  --image kindest/node:v1.37.0@sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5 \
  --wait 180s
kubectl --context kind-dev get nodes
```

If `DOCKER_HOST` or `DOCKER_CONTEXT` is already set, it can override the context
selected above. Check `docker info` before creating the cluster. For a custom
Lighter machine home, set `DOCKER_HOST` to that home's `docker.sock` and unset
`DOCKER_CONTEXT`.

Four vCPUs and 4 GiB were used for the small one- and two-node qualification
workloads. Application requirements determine how much more to allocate:

```sh
lighter config --cpus 4 --memory 8192
lighter restart
```

The memory setting is in MiB and limits the entire Linux VM, including all
Docker containers and kind nodes. The macOS process also needs host buffers,
page tables and stacks. Multiple kind nodes share this same VM budget.

## Local images and access

Build and load your application image before applying its Kubernetes manifest:

```sh
docker build -t my-app:dev .
kind load docker-image my-app:dev --name dev
kubectl --context kind-dev apply -f deployment.yaml
```

Use `imagePullPolicy: IfNotPresent` or `Never` for that local image. For access
from the Mac, use `kubectl port-forward`, or configure kind port mappings when
creating the cluster. For example, this two-node `kind.yaml` maps the Mac's
localhost port 8080 to a Service whose `nodePort` is 30080:

```yaml
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
networking:
  apiServerAddress: 127.0.0.1
nodes:
- role: control-plane
  extraPortMappings:
  - containerPort: 30080
    hostPort: 8080
    listenAddress: 127.0.0.1
- role: worker
```

Pass `--config kind.yaml` along with the pinned image. Pod and Service IPs are
internal to the cluster; use the mapped port or port-forward from macOS.

To mount Mac files, add an `extraMounts` entry to each node that needs them.
The absolute Mac path must be inside a configured Lighter share:

```yaml
  extraMounts:
  - hostPath: /Users/you/project
    containerPath: /workspace
```

A Pod on that node can then mount `/workspace` using a Kubernetes `hostPath`
volume. Host edits become visible through Lighter's shared filesystem; flushed
Pod writes reach the Mac. This qualification checks file visibility and fsync,
not every application's filesystem-watcher API.

These are standard kind configuration options; see the upstream
[configuration guide](https://kind.sigs.k8s.io/docs/user/configuration/) and
[local-image workflow](https://kind.sigs.k8s.io/docs/user/quick-start/#loading-an-image-into-your-cluster).

## Tested scope

The 0.5.0 qualification record (in the worklog) tracks release readiness and
links the evidence. The version matrix uses these exact node digests:

| Kubernetes | kindest/node digest | Topology |
|---|---|---|
| 1.35.8 | `sha256:07b2536e30b803ed61d1677a79df6115f798ce64c80f9e22f6ed45afd09323c0` | One control plane; control plane + worker |
| 1.36.4 | `sha256:099e049362a1526b2db71494e1947aae99bd16290d7c895f2b7ea312e3cbfaed` | One control plane; control plane + worker |
| 1.37.0 | `sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5` | One control plane; control plane + worker |

The scope is Apple Silicon, native arm64 nodes, IPv4, kindnet and default
iptables kube-proxy. An additional two-node 1.37.0 configuration uses
`networking.kubeProxyMode: nftables`. Tests cover creation/deletion, local image
loading, Helm install/upgrade/uninstall, Service DNS, external HTTPS, cross-node
TCP/UDP, host port access, Mac shares, PVCs and VM restart recovery.

Gate m22 (`scripts/gates/m22-kubernetes.sh`) adds two clusters whose network
plugins need more of the kernel than kindnet: k3d as created with no flags, and
kind with Cilium replacing kube-proxy (both below). For each it checks Service
DNS and HTTP from pods on both nodes, names outside the cluster, a deny-all
NetworkPolicy that blocks and then lifts, and a container's own UDP through
the guest beside the cluster.

Kubernetes conformance, HA control planes, dual-stack clusters, CNIs other
than kindnet, flannel and Cilium, LoadBalancer controllers and amd64 nodes are
outside this matrix. The host engine's CRI plugins remain disabled: Kubernetes
uses the containerd inside each node.

## k3d

k3d runs k3s with its defaults: flannel over VXLAN between nodes, and
kube-router enforcing NetworkPolicy. No flags are needed:

```sh
brew install k3d
k3d cluster create dev --agents 1
kubectl get nodes
```

Gate m22 uses k3d 5.9.0 with `rancher/k3s:v1.35.5-k3s1`. Before 0.13.9 the
server restarted in a loop on `failed to create vxlan device: operation not
supported`, and the workarounds (flannel's host-gw backend) left a pod unable
to reach a Service whose endpoint was on its own node and NetworkPolicy
unenforced (#106).

## Cilium

Cilium installs into a kind cluster created without kindnet, with or without
kube-proxy:

```yaml
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes: [{role: control-plane}, {role: worker}]
networking: {disableDefaultCNI: true, kubeProxyMode: none}
```

```sh
kind create cluster --name cilium --config kind.yaml
helm repo add cilium https://helm.cilium.io
helm install cilium cilium/cilium --version 1.20.2 -n kube-system \
  --set kubeProxyReplacement=true \
  --set k8sServiceHost=cilium-control-plane --set k8sServicePort=6443
```

The guest's kernel carries what Cilium's agent checks for at start and loads:
the net_cls classid behind `bpf_get_cgroup_classid()`, BTF for its CO-RE
programs (`/sys/kernel/btf/vmlinux`), VXLAN and GENEVE, the TPROXY, socket
and CT extensions its L7 proxy diverts with, and the BPF socket iterators it
closes client sockets to a deleted Service backend with (#108).

## The guest's own network namespace

The kernel carries bridged netfilter (`br_netfilter`) for the nodes, whose own
network namespaces have it on by default. The guest's own namespace never
does: lighter carries a container's UDP by TPROXY, and with bridged netfilter
on, the kernel runs the IP hooks in bridge context and then drops the socket
TPROXY assigned. Init turns its three switches off and binds them read-only,
so dockerd cannot turn them back on. The one visible effect is that a Docker
network with inter-container traffic off
(`-o com.docker.network.bridge.enable_icc=false`) is refused, as it was before
the kernel carried bridged netfilter.

## Stop, restart and update

`lighter stop` shuts down the VM and its clusters. `lighter start` brings back
the node containers; allow the Kubernetes controllers and Service routes to
recover before using applications. PVC data survives Pod replacement and VM
restart. Deleting a kind cluster deletes its node containers and the data held
inside them:

```sh
kind delete cluster --name dev
```

Lighter updates include its guest kernel and root filesystem as one verified
release. They do not change the Kubernetes version of an existing kind cluster.
To move Kubernetes versions, preserve application data and recreate the cluster
with the chosen node image. The [kind release notes](https://github.com/kubernetes-sigs/kind/releases/tag/v0.33.0)
list the node images built for this kind version.
