import os

from wing.agent import WingAgent


def resolve_path(path: str, agent: WingAgent | None = None) -> str:
    """Resolve a path relative to the agent's cwd, falling back to process cwd.

    Shared by file/search tools so relative paths resolve against the session
    workspace directory.
    """
    if os.path.isabs(path):
        return path
    if agent is not None:
        cwd = agent.state.get("cwd")
        if isinstance(cwd, str):
            return os.path.join(cwd, path)
    return os.path.abspath(path)
