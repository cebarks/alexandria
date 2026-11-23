"""Set up the optional Hermes host contract before importing the provider package."""
import importlib
import sys
from pathlib import Path


def bootstrap_hermes() -> None:
    root = Path(__file__).resolve().parents[1]
    sys.path.insert(0, str(root))
    host = Path.home() / '.hermes' / 'hermes-agent'
    if host.is_dir():
        sys.path.insert(0, str(host))
    try:
        importlib.import_module('agent.memory_provider')
    except ModuleNotFoundError as exc:
        if exc.name not in ('agent', 'agent.memory_provider'):
            raise
        sys.path.insert(0, str(root / 'tests' / '_hermes_stub'))
