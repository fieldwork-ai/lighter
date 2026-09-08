"""Plot filesystem-daemon observations from completed 0.5.0 records."""

import argparse, datetime, gzip, json, pathlib, math
import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

p = argparse.ArgumentParser()
p.add_argument("root", type=pathlib.Path)
p.add_argument("--output", type=pathlib.Path, required=True)
a = p.parse_args()
a.output.mkdir(parents=True, exist_ok=True)
plt.rcParams.update(
    {
        "font.family": "DejaVu Sans",
        "font.size": 10,
        "axes.spines.top": False,
        "axes.spines.right": False,
        "svg.fonttype": "none",
        "svg.hashsalt": "lighter-050",
    }
)


def observer_text(path):
    return (
        path.read_text()
        if path.exists()
        else gzip.decompress(
            path.with_suffix(path.suffix + ".gz").read_bytes()
        ).decode()
    )


for host in ["m1", "m5"]:
    rows = [
        json.loads(l)
        for l in observer_text(a.root / host / "daemon.jsonl").splitlines()
    ]
    valid = [r for r in rows if "error" not in r]
    start = datetime.datetime.fromisoformat(valid[0]["utc"])
    x = [
        (datetime.datetime.fromisoformat(r["utc"]) - start).total_seconds() / 60
        for r in valid
    ]
    fig, axes = plt.subplots(2, 1, figsize=(11, 5.8), sharex=True, layout="constrained")
    for ax, key, label, color in zip(
        axes,
        ["footprint_mib", "cpu_percent"],
        ["Physical footprint (MiB)", "CPU (% of one core)"],
        ["#2463a0", "#b14e23"],
    ):
        values = [r[key] for r in valid]
        if key == "cpu_percent":
            limit = {"m1": 800, "m5": 1800}[host]
            invalid = sum(not (math.isfinite(v) and 0 <= v <= limit) for v in values)
            values = [
                v if math.isfinite(v) and 0 <= v <= limit else float("nan")
                for v in values
            ]
            if invalid:
                ax.set_title(
                    f"{invalid} invalid CPU reading retained in raw data; shown as a gap",
                    loc="left",
                    fontsize=9,
                )
        ax.plot(x, values, color=color, linewidth=1)
        ax.set_ylabel(label)
        ax.set_ylim(bottom=0)
        ax.grid(axis="y", alpha=0.2)
        post = [t for t, r in zip(x, valid) if r["phase"] == "ten-minute observation"]
        if post:
            ax.axvspan(
                min(post),
                max(post),
                color="#66717a",
                alpha=0.15,
                label="10-minute post-suite observation",
            )
    axes[0].legend(loc="upper left", frameon=False)
    axes[-1].set_xlabel("Elapsed time (minutes)")
    fig.suptitle(
        f"{host.upper()} · 0.5.0 release sequence · fseventsd",
        fontweight="bold",
        fontsize=13,
    )
    fig.savefig(a.output / f"fseventsd-{host}.svg", metadata={"Date": None})
    fig.savefig(a.output / f"fseventsd-{host}.png", dpi=150)
    plt.close(fig)
    print(
        host,
        len(valid),
        "samples;",
        len(rows) - len(valid),
        "errors; PIDs",
        sorted({r["pid"] for r in valid}),
    )
