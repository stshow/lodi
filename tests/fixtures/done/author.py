#!/usr/bin/env python3
"""Author the two final-proof commits from a disposable guest's imported package set.

Only the gate's scratch repository is written. The import supplies the distro-specific package
names and invoking account; these declarations supply everything else, without copying /etc
or any home file. The older commit and the changed commit both carry their complete homes.
"""
import hashlib
import json
from pathlib import Path
import sys
import tomllib


def scalar(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=True)
    if isinstance(value, list):
        return "[" + ", ".join(map(scalar, value)) + "]"
    raise ValueError(f"not a TOML scalar: {type(value)}")


def toml(data, prefix=()):
    lines = []
    if prefix:
        lines.append("[" + ".".join(json.dumps(part) for part in prefix) + "]")
    for key, value in data.items():
        if not isinstance(value, dict):
            lines.append(f"{json.dumps(key)} = {scalar(value)}")
    lines.append("")
    for key, value in data.items():
        if isinstance(value, dict):
            lines.append(toml(value, prefix + (key,)))
    return "\n".join(lines)


def author(repo, host, login, iface, locale, sysctl, changed):
    folder = repo / host
    path = folder / "host.toml"
    data = tomllib.loads(path.read_text())
    # Keep the imported exact package declaration and its pins. The invoker already exists;
    # the extra group is created by this apply, not by the harness.
    account = data["users"][login]
    if changed and "lodi-proof" not in account["groups"]:
        account["groups"].append("lodi-proof")
    data["groups"] = {"lodi-proof": {}}
    data["system"] = {"hostname": "hp1-proof" if changed else host,
                      "timezone": "Europe/Berlin" if changed else "UTC",
                      "locale": locale, "keymap": "de" if changed else "us"}
    data["services"] = {"fstrim.timer": "enabled" if changed else "disabled"}
    data["kernel"] = {"parameters": ["lodi_hp1=on"] if changed else [],
                      "modules": ["dummy"] if changed else [],
                      "blacklist": ["pcspkr"] if changed else []}
    data["sysctl"] = {"vm.swappiness": "17" if changed else sysctl}
    data["boot"] = {"loader": "grub", "default": "0", "timeout": 3 if changed else 5}
    data["firewall"] = {"allow": ["22/tcp"]}
    data["network"] = {"confirm_within": 30,
                       "interfaces": {iface: {"dhcp": True,
                                               "addresses": ["10.0.2.50/24"] if changed
                                               else []}}}
    note = "declared\n" if changed else "earlier\n"
    data["etc"] = {"lodi-final-proof": {"text": note}}
    path.write_text(toml(data))
    home = {"home": {"version": "1", "file": {"final-proof-note": {"text": note}}},
            "programs": {"git": {"user": {"name": "gate"}}},
            "tools": {"proof-tool": {
                "version": "1.0.0", "url": "https://proof.test/proof-tool",
                "sha256": hashlib.sha256((Path(__file__).parent / "assets/proof-tool")
                                         .read_bytes()).hexdigest(),
                "format": "binary", "path": ["bin"]}},
            "services": {"lodi-proof.service": {
                "enable": changed, "linger": True,
                "unit": "[Unit]\nDescription=Lodi final proof\n\n[Service]\n"
                        "ExecStart=/bin/sleep infinity\n\n[Install]\n"
                        "WantedBy=default.target\n"}}}
    own = folder / "home" / login
    own.mkdir(parents=True, exist_ok=True)
    (own / "home.toml").write_text(toml(home))
    root = folder / "home" / "root"
    root.mkdir(parents=True, exist_ok=True)
    (root / "home.toml").write_text(toml({
        "home": {"version": "1", "file": {"final-proof-note": {"text": note}}}}))


if __name__ == "__main__":
    repo, host, login, iface, locale, sysctl, mode = sys.argv[1:]
    author(Path(repo), host, login, iface, locale, sysctl, mode == "changed")
