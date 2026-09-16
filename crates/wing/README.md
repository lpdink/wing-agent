# wing-cli

The `wing` binary for [wing-agent](https://github.com/lpdink/wing-agent) — one Rust executable providing three frontends:

- **TUI** (default): interactive, human-in-the-loop session
- **stdio** (`wing -p`): headless, Claude Code compatible NDJSON, so external orchestrators can drive wing
- **Orchestration CLI**: background goals (`wing run` / `wait` / `ps` / `info` / `tail` / `head`) and gateway lifecycle (`wing start` / `stop` / `status`)

Most users want the meta package, which installs this binary together with the Python runtime:

```bash
pip install wing-agent
```

- Documentation: [docs/zh/README.md](https://github.com/lpdink/wing-agent/blob/develop/docs/zh/README.md)
- License: Apache-2.0
