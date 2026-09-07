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

The [0.5.0 qualification record](release-0.5.0.md) tracks release readiness and
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

Kubernetes conformance, HA control planes, dual-stack clusters, custom CNIs,
LoadBalancer/Ingress controllers, amd64 nodes and k3d are outside this matrix.
The host engine's CRI plugins remain disabled: Kubernetes uses the containerd
inside each kind node.

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
