#!/usr/bin/env python3
"""Qualify kind on an already-running, isolated Lighter VM.

Requires explicit LIGHTER_HOME, LIGHTER_GUEST_DIR, DOCKER_HOST and a shared
output directory. Never uses the daily driver's VM or kubeconfig. Output,
including exported cluster logs, is private diagnostic material.
"""

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess as sp
import time
import uuid


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--lighter", required=True)
    ap.add_argument("--expected-version", default="0.5.0")
    ap.add_argument("--kind", required=True)
    ap.add_argument("--kubectl", required=True)
    ap.add_argument("--helm", required=True)
    ap.add_argument("--image", required=True)
    ap.add_argument("--base-image", required=True)
    ap.add_argument("--nodes", type=int, choices=[1, 2], required=True)
    ap.add_argument(
        "--proxy-mode", choices=["iptables", "nftables"], default="iptables"
    )
    ap.add_argument("--memory-pressure", action="store_true")
    a = ap.parse_args()
    home = Path(os.environ.get("LIGHTER_HOME", "")).resolve()
    if (
        not os.environ.get("LIGHTER_HOME")
        or home == (Path.home() / ".lighter").resolve()
    ):
        ap.error("an isolated LIGHTER_HOME is required")
    if os.environ.get("DOCKER_CONTEXT"):
        ap.error("unset DOCKER_CONTEXT")
    if os.environ.get("DOCKER_HOST") != f"unix://{home}/docker.sock":
        ap.error("DOCKER_HOST must select the isolated LIGHTER_HOME socket")
    if "@sha256:" not in a.image or "@sha256:" not in a.base_image:
        ap.error("pin node and application base images by digest")
    os.umask(0o077)
    out = a.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    share = out / "share"
    share.mkdir()
    (share / "token").write_text("mac-token")
    env = dict(
        os.environ,
        KUBECONFIG=str(out / "kubeconfig"),
        KIND_EXPERIMENTAL_PROVIDER="docker",
    )
    name = "lighter-q-" + uuid.uuid4().hex[:10]
    ns = "qualification"
    node = name + ("-worker" if a.nodes == 2 else "-control-plane")
    image = name + ":local"
    results = []
    index = 0

    def cmd(args, timeout=60, required=True):
        nonlocal index
        index += 1
        with (out / "commands.jsonl").open("a") as f:
            f.write(json.dumps(dict(index=index, args=args)) + "\n")
        try:
            p = sp.run(args, env=env, capture_output=True, text=True, timeout=timeout)
            (out / f"command-{index:03d}.log").write_text(p.stdout + p.stderr)
        except sp.TimeoutExpired as e:
            (out / f"command-{index:03d}.log").write_bytes(
                (e.stdout or b"") + (e.stderr or b"")
            )
            raise RuntimeError(f"command {index} timed out: {args[0]}") from e
        if required and p.returncode:
            raise RuntimeError(
                f"command {index} exited {p.returncode}: {p.stderr[-1500:]}"
            )
        return p

    def k(*args, timeout=60):
        return cmd([a.kubectl, "--request-timeout=10s", *args], timeout).stdout

    def checked(label, action):
        try:
            action()
            results.append(dict(check=label, passed=True))
            print("PASS:", label, flush=True)
        except Exception as e:
            results.append(dict(check=label, passed=False, error=str(e)))
            print("FAIL:", label, str(e), flush=True)
            raise
        finally:
            (out / "results.json").write_text(json.dumps(results, indent=2) + "\n")

    def equal(actual, expected):
        if actual.strip() != expected:
            raise AssertionError(f"expected {expected!r}, received {actual!r}")

    def free_port(kind):
        with socket.socket(socket.AF_INET, kind) as s:
            s.bind(("127.0.0.1", 0))
            return s.getsockname()[1]

    def pod_python(code, target="pod/client"):
        return k("-n", ns, "exec", target, "--", "python3", "-c", code)

    def eventually(action, seconds=60):
        """Bounded controller convergence; every failed attempt remains in the log."""
        deadline = time.monotonic() + seconds
        last = None
        while time.monotonic() < deadline:
            try:
                action()
                return
            except Exception as error:
                last = error
                time.sleep(1)
        raise RuntimeError(f"convergence exceeded {seconds}s: {last}")

    def digest(path):
        with path.open("rb") as source:
            return hashlib.file_digest(source, "sha256").hexdigest()

    def service_check():
        equal(
            pod_python(
                "import urllib.request; print(urllib.request.urlopen('http://web',timeout=3).read().decode())"
            ),
            "kind-ok",
        )
        equal(
            cmd(
                [
                    "curl",
                    "--noproxy",
                    "*",
                    "-fsS",
                    "--max-time",
                    "3",
                    f"http://127.0.0.1:{tcp_port}/",
                ]
            ).stdout,
            "kind-ok",
        )

    tcp_port, udp_port, forward_port = (
        free_port(t)
        for t in [socket.SOCK_STREAM, socket.SOCK_DGRAM, socket.SOCK_STREAM]
    )
    nodes = [
        dict(
            role="control-plane",
            extraPortMappings=[
                dict(containerPort=30080, hostPort=tcp_port, listenAddress="127.0.0.1"),
                dict(
                    containerPort=30081,
                    hostPort=udp_port,
                    listenAddress="127.0.0.1",
                    protocol="UDP",
                ),
            ],
        )
    ]
    if a.nodes == 2:
        nodes.append(dict(role="worker"))
    for item in nodes:
        item["extraMounts"] = [dict(hostPath=str(share), containerPath="/host-fixture")]
    config = dict(
        kind="Cluster",
        apiVersion="kind.x-k8s.io/v1alpha4",
        networking=dict(apiServerAddress="127.0.0.1", kubeProxyMode=a.proxy_mode),
        nodes=nodes,
    )
    (out / "kind.json").write_text(json.dumps(config, indent=2))
    base_version = cmd([a.lighter, "--version"]).stdout
    if base_version.strip() != f"lighter {a.expected_version}":
        raise RuntimeError(f"qualification requires the {a.expected_version} CLI")
    guest = Path(os.environ["LIGHTER_GUEST_DIR"])
    metadata = dict(
        lighter=base_version.strip(),
        lighter_sha256=digest(Path(a.lighter)),
        source=cmd(["git", "rev-parse", "HEAD"]).stdout.strip(),
        kind=cmd([a.kind, "version"]).stdout.strip(),
        kubectl=json.loads(
            cmd([a.kubectl, "version", "--client", "-o", "json"]).stdout
        ),
        image=a.image,
        base_image=a.base_image,
        nodes=a.nodes,
        proxy_mode=a.proxy_mode,
        guest_sha256={n: digest(guest / n) for n in ["Image", "rootfs.ext4"]},
        vm_config=json.loads((home / "config.json").read_text()),
    )
    (out / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    created = False
    passed = False
    try:
        created = True  # --retain can leave nodes even if create itself fails.
        checked(
            "normal cluster creation",
            lambda: cmd(
                [
                    a.kind,
                    "create",
                    "cluster",
                    "--name",
                    name,
                    "--config",
                    str(out / "kind.json"),
                    "--image",
                    a.image,
                    "--wait",
                    "180s",
                    "--retain",
                ],
                420,
            ),
        )
        checked(
            "all nodes Ready",
            lambda: k(
                "wait",
                "--for=condition=Ready",
                "nodes",
                "--all",
                "--timeout=180s",
                timeout=200,
            ),
        )
        versions = json.loads(k("version", "-o", "json"))
        expected = a.image.split("@")[0].rsplit(":", 1)[1]
        equal(versions["serverVersion"]["gitVersion"], expected)
        metadata["kubernetes"] = versions
        (out / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
        app = out / "app"
        app.mkdir()
        (app / "server.py").write_text("""import http.server,os,socket,threading
class Handler(http.server.BaseHTTPRequestHandler):
 def do_GET(self):
  body=os.environ.get("RESPONSE","kind-ok").encode()
  self.send_response(200); self.send_header("Content-Length",str(len(body))); self.end_headers(); self.wfile.write(body)
def udp():
 s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.bind(("0.0.0.0",8081))
 while True:
  data,addr=s.recvfrom(65535);s.sendto(data,addr)
threading.Thread(target=udp,daemon=True).start()
print("server-started",flush=True)
http.server.ThreadingHTTPServer(("0.0.0.0",8080),Handler).serve_forever()
""")
        (app / "Dockerfile").write_text(
            f'FROM {a.base_image}\nCOPY server.py /server.py\nCMD ["python3", "-u", "/server.py"]\n'
        )
        checked(
            "local image build",
            lambda: cmd(["docker", "build", "-t", image, str(app)], 300),
        )
        checked(
            "load local image into every node",
            lambda: cmd([a.kind, "load", "docker-image", image, "--name", name], 180),
        )
        container = dict(
            name="web",
            image=image,
            imagePullPolicy="Never",
            resources=dict(
                requests=dict(cpu="10m", memory="32Mi"), limits=dict(memory="128Mi")
            ),
            readinessProbe=dict(httpGet=dict(path="/", port=8080), periodSeconds=1),
            volumeMounts=[
                dict(name="data", mountPath="/data"),
                dict(name="mac", mountPath="/mac"),
            ],
        )
        deployment = dict(
            apiVersion="apps/v1",
            kind="Deployment",
            metadata=dict(name="web", namespace=ns),
            spec=dict(
                replicas=1,
                selector=dict(matchLabels=dict(app="web")),
                template=dict(
                    metadata=dict(labels=dict(app="web")),
                    spec=dict(
                        nodeSelector={"kubernetes.io/hostname": node},
                        tolerations=[dict(operator="Exists")],
                        containers=[container],
                        volumes=[
                            dict(
                                name="data",
                                persistentVolumeClaim=dict(claimName="data"),
                            ),
                            dict(
                                name="mac",
                                hostPath=dict(path="/host-fixture", type="Directory"),
                            ),
                        ],
                    ),
                ),
            ),
        )
        items = [
            dict(apiVersion="v1", kind="Namespace", metadata=dict(name=ns)),
            dict(
                apiVersion="v1",
                kind="PersistentVolumeClaim",
                metadata=dict(name="data", namespace=ns),
                spec=dict(
                    accessModes=["ReadWriteOnce"],
                    resources=dict(requests=dict(storage="64Mi")),
                ),
            ),
            deployment,
            dict(
                apiVersion="v1",
                kind="Service",
                metadata=dict(name="web", namespace=ns),
                spec=dict(
                    type="NodePort",
                    selector=dict(app="web"),
                    ports=[
                        dict(name="http", port=80, targetPort=8080, nodePort=30080),
                        dict(
                            name="udp",
                            port=8081,
                            targetPort=8081,
                            nodePort=30081,
                            protocol="UDP",
                        ),
                    ],
                ),
            ),
            dict(
                apiVersion="v1",
                kind="Pod",
                metadata=dict(name="client", namespace=ns),
                spec=dict(
                    nodeName=name + "-control-plane",
                    tolerations=[dict(operator="Exists")],
                    containers=[
                        dict(
                            name="client",
                            image=image,
                            imagePullPolicy="Never",
                            command=["sleep", "7200"],
                        )
                    ],
                ),
            ),
        ]
        (out / "app.json").write_text(
            json.dumps(dict(apiVersion="v1", kind="List", items=items), indent=2)
        )
        checked(
            "deploy workload and PVC", lambda: k("apply", "-f", str(out / "app.json"))
        )
        checked(
            "application Ready",
            lambda: k(
                "-n",
                ns,
                "rollout",
                "status",
                "deployment/web",
                "--timeout=180s",
                timeout=200,
            ),
        )
        checked(
            "client Ready",
            lambda: k(
                "-n",
                ns,
                "wait",
                "--for=condition=Ready",
                "pod/client",
                "--timeout=180s",
                timeout=200,
            ),
        )
        checked(
            "initial Service routing convergence", lambda: eventually(service_check)
        )
        checked("Service DNS, cross-node TCP and host NodePort", service_check)
        checked(
            "external DNS and HTTPS",
            lambda: pod_python(
                "import socket,urllib.request; assert socket.getaddrinfo('example.com',443); assert urllib.request.urlopen('https://example.com',timeout=10).status==200"
            ),
        )
        pod_ip = json.loads(k("-n", ns, "get", "pods", "-l", "app=web", "-o", "json"))[
            "items"
        ][0]["status"]["podIP"]
        checked(
            "direct Pod IP",
            lambda: equal(
                pod_python(
                    f"import urllib.request;print(urllib.request.urlopen('http://{pod_ip}:8080',timeout=3).read().decode())"
                ),
                "kind-ok",
            ),
        )
        checked(
            "cluster UDP",
            lambda: pod_python(
                "import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.settimeout(5);s.sendto(b'udp-test',('web',8081));assert s.recv(32)==b'udp-test'"
            ),
        )

        def host_udp():
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
                s.settimeout(5)
                s.sendto(b"host-udp", ("127.0.0.1", udp_port))
                assert s.recv(32) == b"host-udp"

        checked("host UDP NodePort", host_udp)
        checked(
            "Mac share read/write",
            lambda: pod_python(
                "from pathlib import Path;assert Path('/mac/token').read_text()=='mac-token';import os;f=open('/mac/from-pod','w');f.write('guest-token');f.flush();os.fsync(f.fileno());f.close();Path('/data/token').write_text('persistent-token')",
                "deployment/web",
            ),
        )
        checked(
            "flushed Pod write visible on Mac",
            lambda: equal((share / "from-pod").read_text(), "guest-token"),
        )
        (share / "token").write_text("mac-updated")
        checked(
            "updated Mac file visible",
            lambda: eventually(
                lambda: equal(
                    pod_python(
                        "from pathlib import Path;print(Path('/mac/token').read_text())",
                        "deployment/web",
                    ),
                    "mac-updated",
                ),
                10,
            ),
        )
        checked(
            "pod logs",
            lambda: equal(
                k("-n", ns, "logs", "deployment/web").splitlines()[0], "server-started"
            ),
        )
        with (out / "port-forward.log").open("w") as log:
            forward = sp.Popen(
                [
                    a.kubectl,
                    "-n",
                    ns,
                    "port-forward",
                    "service/web",
                    f"{forward_port}:80",
                    "--address",
                    "127.0.0.1",
                ],
                env=env,
                stdout=log,
                stderr=log,
            )
            try:
                for _ in range(50):
                    if "Forwarding from" in (out / "port-forward.log").read_text():
                        break
                    time.sleep(0.1)
                checked(
                    "kubectl port-forward",
                    lambda: equal(
                        cmd(
                            [
                                "curl",
                                "--noproxy",
                                "*",
                                "-fsS",
                                "--max-time",
                                "5",
                                f"http://127.0.0.1:{forward_port}",
                            ]
                        ).stdout,
                        "kind-ok",
                    ),
                )
            finally:
                forward.terminate()
                try:
                    forward.wait(5)
                except sp.TimeoutExpired:
                    forward.kill()
                    forward.wait()
        chart = out / "chart"
        (chart / "templates").mkdir(parents=True)
        (chart / "Chart.yaml").write_text(
            "apiVersion: v2\nname: lighter-qualification\nversion: 0.1.0\n"
        )
        hd = copy.deepcopy(deployment)
        hd["metadata"]["name"] = "helm-web"
        hd["spec"]["selector"]["matchLabels"]["app"] = "helm-web"
        ht = hd["spec"]["template"]
        ht["metadata"]["labels"]["app"] = "helm-web"
        ht["spec"].pop("volumes")
        ht["spec"]["containers"][0].pop("volumeMounts")
        ht["spec"]["containers"][0]["env"] = [
            dict(name="RESPONSE", value="{{ .Values.response }}")
        ]
        hs = dict(
            apiVersion="v1",
            kind="Service",
            metadata=dict(name="helm-web", namespace=ns),
            spec=dict(
                selector=dict(app="helm-web"), ports=[dict(port=80, targetPort=8080)]
            ),
        )
        (chart / "templates/deployment.yaml").write_text(json.dumps(hd))
        (chart / "templates/service.yaml").write_text(json.dumps(hs))
        for response in ["helm-v1", "helm-v2"]:
            checked(
                "Helm install/upgrade " + response,
                lambda response=response: cmd(
                    [
                        a.helm,
                        "upgrade",
                        "--install",
                        "qualification",
                        str(chart),
                        "-n",
                        ns,
                        "--set-string",
                        "response=" + response,
                        "--wait",
                        "--timeout",
                        "180s",
                    ],
                    210,
                ),
            )
            checked(
                "Helm workload " + response,
                lambda response=response: eventually(
                    lambda: equal(
                        pod_python(
                            "import urllib.request;print(urllib.request.urlopen('http://helm-web',timeout=5).read().decode())"
                        ),
                        response,
                    )
                ),
            )
        checked(
            "Helm uninstall",
            lambda: cmd(
                [
                    a.helm,
                    "uninstall",
                    "qualification",
                    "-n",
                    ns,
                    "--wait",
                    "--timeout",
                    "120s",
                ],
                140,
            ),
        )
        if a.memory_pressure:
            from kind_pressure import qualify

            checked(
                "mixed Docker/build/Kubernetes memory pressure",
                lambda: qualify(
                    home=home,
                    out=out,
                    cmd=cmd,
                    k=k,
                    image=image,
                    base_image=a.base_image,
                    node=node,
                    namespace=ns,
                    service_check=service_check,
                ),
            )
        old = json.loads(k("-n", ns, "get", "pods", "-l", "app=web", "-o", "json"))[
            "items"
        ][0]["metadata"]["uid"]
        k(
            "-n",
            ns,
            "delete",
            "pods",
            "-l",
            "app=web",
            "--wait=true",
            "--timeout=60s",
            timeout=80,
        )
        k(
            "-n",
            ns,
            "rollout",
            "status",
            "deployment/web",
            "--timeout=180s",
            timeout=200,
        )
        new = json.loads(k("-n", ns, "get", "pods", "-l", "app=web", "-o", "json"))[
            "items"
        ][0]["metadata"]["uid"]
        assert old != new
        checked(
            "PVC survives Pod replacement",
            lambda: equal(
                pod_python(
                    "from pathlib import Path;print(Path('/data/token').read_text())",
                    "deployment/web",
                ),
                "persistent-token",
            ),
        )
        checked("VM stop", lambda: cmd([a.lighter, "stop"], 120))
        checked(
            "VM restart", lambda: cmd([a.lighter, "start", "--timeout", "120"], 140)
        )

        def recovery():
            deadline = time.monotonic() + 180
            while time.monotonic() < deadline:
                try:
                    service_check()
                    return
                except Exception:
                    time.sleep(2)
            raise RuntimeError("end-to-end service did not recover after restart")

        checked("traffic recovers after VM restart", recovery)
        checked(
            "PVC survives VM restart",
            lambda: equal(
                pod_python(
                    "from pathlib import Path;print(Path('/data/token').read_text())",
                    "deployment/web",
                ),
                "persistent-token",
            ),
        )
        checked("host UDP after restart", host_udp)
        checked(
            "ordinary Docker after kind",
            lambda: equal(
                cmd(
                    [
                        "docker",
                        "run",
                        "--rm",
                        "--entrypoint",
                        "printf",
                        a.base_image,
                        "docker-ok",
                    ]
                ).stdout,
                "docker-ok",
            ),
        )
        passed = True
    finally:
        if created:
            try:
                cmd(
                    [
                        a.kind,
                        "export",
                        "logs",
                        str(out / "cluster-logs"),
                        "--name",
                        name,
                    ],
                    120,
                    False,
                )
            except Exception as e:
                print("Log export failed:", e, flush=True)
            try:
                cmd([a.kind, "delete", "cluster", "--name", name], 120)
                leftovers = cmd(
                    [
                        "docker",
                        "ps",
                        "-aq",
                        "--filter",
                        "label=io.x-k8s.kind.cluster=" + name,
                    ]
                ).stdout.strip()
                if leftovers:
                    raise RuntimeError("cluster containers remain")
                results.append(dict(check="cluster cleanup", passed=True))
            except Exception as e:
                passed = False
                results.append(
                    dict(check="cluster cleanup", passed=False, error=str(e))
                )
        (out / "results.json").write_text(json.dumps(results, indent=2) + "\n")
        (out / "exit-code").write_text("0\n" if passed else "1\n")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
