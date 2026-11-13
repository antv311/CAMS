"""aircd - Python client library for aircd IRC server."""

from aircd.client import AircdClient, AircdHttpClient, AircdHttpError, Group, GroupMember

__all__ = ["AircdClient", "AircdHttpClient", "AircdHttpError", "Group", "GroupMember"]
__version__ = "0.1.0"
