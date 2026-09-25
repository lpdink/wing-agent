# wing-gateway

The Python runtime behind [wing](https://github.com/lpdink/wing-agent): session lifecycle, context window tracking and compaction, tool registry, event bus, and the FastAPI + WebSocket gateway the `wing` frontends talk to.

Most users want the meta package instead — it installs the `wing` binary and this runtime together:

```bash
pip install wing-agent
```

`wing-gateway` is published separately for deployments that only need the runtime. It is started directly (`wing-gateway`), or by the CLI (`wing start`).

- Documentation: [docs/zh/README.md](https://github.com/lpdink/wing-agent/blob/develop/docs/zh/README.md)
- HTTP / WebSocket protocol: [docs/dev/http-api.md](https://github.com/lpdink/wing-agent/blob/develop/docs/dev/http-api.md)
- License: Apache-2.0
