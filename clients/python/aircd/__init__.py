"""aircd - Python client library for aircd IRC server."""

from aircd.bionic import BionicAgent, BionicConfig, BionicError, RunResult
from aircd.client import AircdClient, AircdHttpClient, AircdHttpError, Group, GroupMember

__all__ = [
    "AircdClient",
    "AircdHttpClient",
    "AircdHttpError",
    "BionicAgent",
    "BionicConfig",
    "BionicError",
    "Group",
    "GroupMember",
    "RunResult",
]
__version__ = "0.1.0"
