"""Onboards a fresh Home Assistant and adds ZHA on a serial radio, through
Home Assistant's own HTTP API, the way its frontend does. Idempotent: an
onboarded instance is signed in to, and an existing ZHA entry is reported.
For gate m15-usb part two.

    ha-zha-setup.py <base url> <credentials file> <device path> <radio type>
"""
import json
import pathlib
import secrets
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE, CREDENTIALS, DEVICE, RADIO = sys.argv[1:5]
CLIENT = BASE + "/"


def call(method, path, body=None, token=None, form=False):
    data = None
    headers = {}
    if body is not None:
        if form:
            data = urllib.parse.urlencode(body).encode()
            headers["Content-Type"] = "application/x-www-form-urlencoded"
        else:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
    if token:
        headers["Authorization"] = "Bearer " + token
    request = urllib.request.Request(BASE + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            text = response.read().decode()
            return json.loads(text) if text else None
    except urllib.error.HTTPError as e:
        raise SystemExit(f"{method} {path}: {e.code} {e.read().decode()[:300]}")


def token_from_code(code):
    return call("POST", "/auth/token", {"grant_type": "authorization_code", "code": code, "client_id": CLIENT}, form=True)["access_token"]


def sign_in(username, password):
    flow = call("POST", "/auth/login_flow", {"client_id": CLIENT, "handler": ["homeassistant", None], "redirect_uri": CLIENT})
    done = call("POST", f"/auth/login_flow/{flow['flow_id']}", {"client_id": CLIENT, "username": username, "password": password})
    return token_from_code(done["result"])


def onboard():
    # Once onboarding is done, a restarted Home Assistant no longer serves
    # its onboarding view at all.
    try:
        with urllib.request.urlopen(BASE + "/api/onboarding", timeout=30) as r:
            steps = {s["step"]: s["done"] for s in json.loads(r.read())}
    except urllib.error.HTTPError as e:
        if e.code != 404:
            raise
        steps = {"user": True, "core_config": True, "analytics": True, "integration": True}
    creds = pathlib.Path(CREDENTIALS)
    if not steps.get("user"):
        password = secrets.token_urlsafe(18)
        creds.write_text(json.dumps({"username": "lighter", "password": password}))
        creds.chmod(0o600)
        code = call("POST", "/api/onboarding/users", {"client_id": CLIENT, "name": "lighter", "username": "lighter", "password": password, "language": "en"})["auth_code"]
        token = token_from_code(code)
    else:
        saved = json.loads(creds.read_text())
        token = sign_in(saved["username"], saved["password"])
    if not steps.get("core_config"):
        call("POST", "/api/onboarding/core_config", {}, token)
    if not steps.get("analytics"):
        call("POST", "/api/onboarding/analytics", {}, token)
    if not steps.get("integration"):
        call("POST", "/api/onboarding/integration", {"client_id": CLIENT, "redirect_uri": CLIENT}, token)
    return token


def zha(token):
    for entry in call("GET", "/api/config/config_entries/entry", token=token):
        if entry["domain"] == "zha":
            return entry
    step = call("POST", "/api/config/config_entries/flow", {"handler": "zha", "show_advanced_options": True}, token)
    # Walk the flow: each form is answered from what it asks for.
    answers = {
        "path": DEVICE,
        "radio_type": RADIO,
        "baudrate": 2000000 if RADIO == "blz" else 115200,
        "flow_control": None,
    }
    for _ in range(12):
        if step["type"] == "create_entry":
            return step["result"]
        if step["type"] == "menu":
            options = step["menu_options"]
            choice = next((o for o in ("choose_setup_strategy", "setup_strategy_recommended", "form_new_network", "manual_pick_radio_type") if o in options), options[0])
            step = call("POST", f"/api/config/config_entries/flow/{step['flow_id']}", {"next_step_id": choice}, token)
            continue
        if step["type"] == "form":
            fields = [f["name"] for f in step.get("data_schema", [])]
            data = {}
            for f in fields:
                if f == "path" and step["step_id"] == "choose_serial_port":
                    # Offered ports are labels; a manual entry takes a path.
                    schema = next(s for s in step["data_schema"] if s["name"] == "path")
                    options = schema.get("options") or []
                    manual = next((o for o in options if "manual" in str(o).lower()), None)
                    data[f] = manual[0] if isinstance(manual, list) else (manual or DEVICE)
                elif f == "radio_type":
                    # Offered as labels ("EZSP = Silicon Labs ..."): the one
                    # that names the radio type.
                    schema = next(s for s in step["data_schema"] if s["name"] == f)
                    labels = [o[0] if isinstance(o, list) else o for o in schema.get("options") or []]
                    match = [l for l in labels if l.lower().split(" =")[0] == RADIO.lower()]
                    if not match:
                        print(f"no {RADIO} radio type in this Home Assistant's ZHA; it offers: {[l.split(' =')[0] for l in labels]}")
                        sys.exit(3)
                    data[f] = match[0]
                elif f in answers and answers[f] is not None:
                    data[f] = answers[f]
            step = call("POST", f"/api/config/config_entries/flow/{step['flow_id']}", data, token)
            continue
        if step["type"] == "progress":
            time.sleep(3)
            step = call("GET", f"/api/config/config_entries/flow/{step['flow_id']}", token=token)
            continue
        raise SystemExit(f"ZHA flow stopped: {json.dumps(step)[:400]}")
    raise SystemExit("ZHA flow did not finish")


token = onboard()
entry = zha(token)
time.sleep(5)
for e in call("GET", "/api/config/config_entries/entry", token=token):
    if e["domain"] == "zha":
        print(json.dumps({"title": e["title"], "state": e["state"]}))
