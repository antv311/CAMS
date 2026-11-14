"""aircd - Python client library for aircd IRC server."""

from aircd.bionic import BionicAgent, BionicConfig, BionicError, MaxTurnsError, RunResult
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
    "MaxTurnsError",
    "RunResult",
]
__version__ = "0.1.0"
