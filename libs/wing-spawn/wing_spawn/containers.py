"""Docker container management for wing-spawn.

Brings up a disposable tool container that registers its tools to the gateway
under a unique `client_id` namespace, then tears it down.

The spawned container reuses the *parent's* network stack (`--network
container:<host>`), so it can resolve the gateway and every compose service
without any extra wiring. Key mounts (workspace, docker.sock, SSH keys) are
carried over from the parent so the child has the same filesystem context.
"""

from __future__ import annotations

import asyncio
import logging
import os
import subprocess
from typing import Any

from wing_sdk.http_client import GatewayClient

log = logging.getLogger("wing-spawn.containers")

# Standard tool names the devbox tool-host registers (mirror of
# wing_sdk.tools.register_standard_tools).
STANDARD_TOOLS = ["Bash", "Read", "Write", "Edit", "Glob", "Grep"]

# Default image: same registry/tag as the parent devbox container.
DEFAULT_IMAGE = os.environ.get(
    "WING_SPAWN_IMAGE",
    f"{os.environ.get('REGISTRY', 'registry.cn-hangzhou.aliyuncs.com/8bitpd')}"
    f"/wing-devbox:{os.environ.get('TAG', 'develop')}",
)


def _hostname() -> str:
    return os.environ.get("HOSTNAME", "") or subprocess.run(
        ["hostname"], capture_output=True, text=True, check=True
    ).stdout.strip()


def _docker(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["docker", *args], capture_output=True, text=True, check=check
    )


def _parent_mounts() -> list[str]:
    """Reuse the parent container's key mounts (workspace volume + docker.sock).

    Inspects the parent container (identified by hostname) and returns a list
    of `-v` arguments that mirror its persistent mounts into the child.
    """
    args: list[str] = []
    host = _hostname()
    if not host:
        return args
    try:
        proc = _docker(
            "inspect",
            host,
            "--format",
            "{{range .Mounts}}{{.Type}}|{{.Name}}|{{.Source}}|{{.Destination}}"
            "{{println}}{{end}}",
        )
        if proc.returncode != 0:
            return args
    except Exception:
        return args

    for line in proc.stdout.strip().splitlines():
        if not line:
            continue
        mtype, name, source, dest = line.split("|")
        if mtype == "volume" and name:
            # Persistent volume (e.g. wing_workspace -> /workspace)
            args += ["-v", f"{name}:{dest}"]
        elif mtype == "bind" and source == "/var/run/docker.sock":
            args += ["-v", f"{source}:{dest}"]
    return args


def spawn_tool_container(
    client_id: str,
    *,
    image: str | None = None,
    gateway_url: str,
    tool_key: str | None,
    workspace: str | None = None,
    network: str | None = None,
    extra_env: dict[str, str] | None = None,
) -> str:
    """Boot a tool container and return its container ID.

    Args:
        client_id: unique namespace the host registers under.
        image: tool-host image tag (default: DEFAULT_IMAGE).
        gateway_url: gateway base URL the host should connect to.
        tool_key: tool_runtime API key (or None if auth is off).
        workspace: host path of the workspace (informational; the child always
            reuses the parent's persistent workspace volume).
        network: docker network to join. Default: share the parent's stack.
        extra_env: extra environment variables for the container.
    """
    image = image or DEFAULT_IMAGE
    cmd: list[str] = [
        "run",
        "-d",
        "--name",
        f"wing-spawn-{client_id}",
        "--label",
        "wing-spawn=1",
    ]

    # Network: default to sharing the parent's stack (no wiring needed).
    if network:
        cmd += ["--network", network]
    else:
        cmd += ["--network", f"container:{_hostname()}"]

    # Mounts: mirror the parent's persistent volume + docker.sock + SSH keys.
    cmd += _parent_mounts()
    if os.path.isdir("/root/.ssh"):
        cmd += ["-v", "/root/.ssh:/root/.ssh:ro"]

    # Environment: gateway + identity + tool registration.
    env: dict[str, str] = {
        "WING_GATEWAY_URL": gateway_url,
        "TOOL_CLIENT_ID": client_id,
        "WING_WORKSPACE": workspace or os.environ.get("WING_WORKSPACE", "/workspace"),
        "WING_REPO": os.environ.get("WING_REPO", "lpdink/wing-agent"),
        "GIT_USER_NAME": os.environ.get("GIT_USER_NAME", ""),
        "GIT_USER_EMAIL": os.environ.get("GIT_USER_EMAIL", ""),
        "GITHUB_TOKEN": os.environ.get("GITHUB_TOKEN", ""),
    }
    if tool_key:
        env["WING_API_KEY"] = tool_key
    if extra_env:
        env.update(extra_env)
    for k, v in env.items():
        if v:
            cmd += ["-e", f"{k}={v}"]

    cmd.append(image)

    log.info("spawning tool container: %s", " ".join(cmd))
    proc = _docker(*cmd)
    if proc.returncode != 0:
        raise RuntimeError(f"docker run failed: {proc.stderr.strip()}")
    return proc.stdout.strip()


async def wait_for_tools(
    gateway_url: str,
    client_id: str,
    api_key: str | None,
    *,
    timeout: float = 60.0,
    poll: float = 1.0,
) -> list[str]:
    """Wait until the gateway sees the client's standard tools registered.

    Returns the registered tool refs (e.g. `["<client_id>.Bash", ...]`).
    """
    deadline = asyncio.get_running_loop().time() + timeout
    async with GatewayClient(gateway_url, api_key) as http:
        while True:
            try:
                tools = await http.list_tools()
                refs = _tool_refs(tools)
                registered = sorted(
                    r for r in refs if r.startswith(f"{client_id}.")
                )
                if registered:
                    return registered
            except Exception as e:  # gateway may not be up yet
                log.debug("waiting for gateway: %s", e)
            if asyncio.get_running_loop().time() >= deadline:
                raise TimeoutError(
                    f"tool container '{client_id}' did not register within "
                    f"{timeout:.0f}s (gateway at {gateway_url})"
                )
            await asyncio.sleep(poll)


def _tool_refs(payload: Any) -> list[str]:
    """Normalise the /api/tools response into a list of ref strings."""
    if isinstance(payload, dict):
        for key in ("tools", "items", "refs"):
            if isinstance(payload.get(key), list):
                payload = payload[key]
                break
    if not isinstance(payload, list):
        return []
    refs: list[str] = []
    for item in payload:
        if isinstance(item, str):
            refs.append(item)
        elif isinstance(item, dict):
            ns = item.get("namespace") or item.get("client_id") or ""
            name = item.get("name") or item.get("llm_name") or ""
            if ns and name:
                refs.append(f"{ns}.{name}")
    return refs


def cleanup_container(client_id: str) -> None:
    """Force-remove the spawned container (idempotent)."""
    name = f"wing-spawn-{client_id}"
    _docker("rm", "-f", name, check=False)
    log.info("removed container %s", name)


def cleanup_all() -> int:
    """Remove every leftover wing-spawn container (label `wing-spawn=1`).

    Returns the number of containers removed.
    """
    proc = _docker(
        "ps", "-aq", "--filter", "label=wing-spawn=1", check=False
    )
    ids = [i for i in proc.stdout.split() if i]
    if not ids:
        return 0
    _docker("rm", "-f", *ids, check=False)
    log.info("removed %d leftover wing-spawn container(s)", len(ids))
    return len(ids)