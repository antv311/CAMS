"""Git workspace snapshots and automatic rollback for Bionic tool calls.

With ``--rollback`` (``BionicConfig.rollback``), ``run_agent`` snapshots the
workspace before every mutating tool call (``run_command``, ``write_file``,
``edit_file``, ``apply_patch`` and mutating plugin tools). When the call fails
(an ``error`` result, a non-zero ``exit_code``, a jail or approval denial) or a
``review`` callback rejects it, the workspace is put back exactly as it was
before the call, and the tool result tells the model so.

How a snapshot works, without touching the user's branch, index or stash list:

* every file under the workspace root that git would see (tracked files plus
  untracked files that are not ignored) is hashed into a tree through a
  private index file (``GIT_INDEX_FILE``); content filters and CRLF
  conversion are switched off, so the snapshot is byte for byte;
* the tree is wrapped in a commit and kept under ``refs/bionic/snapshot``
  (the newest one) so ``git restore --source=refs/bionic/snapshot`` also
  works by hand;
* a rollback first stores what it is about to throw away as a stash-shaped
  commit on top of the snapshot under ``refs/bionic/rollbacks/<id>``
  (``git stash show -p <sha>`` is the discarded change; bring it back with
  ``git stash show -p <sha> | git apply``), then restores the snapshot tree into the
  working tree, deleting files the tool created, again through a private
  index. ``HEAD`` and the real index are put back too if the tool moved them.

Ignored files (build output, ``target/``, ``node_modules`` in ``.gitignore``)
are neither snapshotted nor restored. Outside a git work tree snapshots are
unavailable and tool calls run unprotected (the run logs a warning once).
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import uuid
import weakref
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Optional

EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
SNAPSHOT_REF = "refs/bionic/snapshot"
ROLLBACK_REF_PREFIX = "refs/bionic/rollbacks/"
GIT_TIMEOUT_SECONDS = 120.0
_IDENTITY = ("-c", "user.name=aircd-bionic", "-c", "user.email=bionic@aircd.invalid")
# Byte-exact snapshots: no autocrlf, no .gitattributes filters/eol from the
# work tree (GIT_ATTR_SOURCE points attribute lookup at the empty tree).
_RAW = ("-c", "core.autocrlf=false", "-c", "core.safecrlf=false", "-c", "core.fsmonitor=false")


class SnapshotError(RuntimeError):
    """A git snapshot or restore step failed."""


@dataclass
class Snapshot:
    """The workspace state captured before one tool call."""

    id: str
    tree: str
    commit: str
    head: Optional[str]
    head_ref: Optional[str]
    index_tree: Optional[str]
    label: str = ""


@dataclass
class RollbackReport:
    """What :meth:`GitSnapshotter.restore` changed."""

    restored: bool
    reason: str
    snapshot: str
    changed: list[str] = field(default_factory=list)
    saved_ref: Optional[str] = None
    saved_commit: Optional[str] = None
    head_restored: bool = False
    index_restored: bool = False
    warnings: list[str] = field(default_factory=list)

    def as_dict(self) -> dict[str, Any]:
        out: dict[str, Any] = {
            "restored": self.restored,
            "reason": self.reason,
            "snapshot": self.snapshot,
            "changed": self.changed,
        }
        if self.saved_ref:
            out["saved_ref"] = self.saved_ref
            out["saved_commit"] = self.saved_commit
        if self.head_restored:
            out["head_restored"] = True
        if self.index_restored:
            out["index_restored"] = True
        if self.warnings:
            out["warnings"] = self.warnings
        return out


def _run_git(
    args: list[str],
    cwd: str,
    *,
    env: Optional[dict[str, str]] = None,
    input_text: Optional[str] = None,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    try:
        proc = subprocess.run(
            ["git", *args],
            cwd=cwd,
            env=env,
            input=input_text,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=GIT_TIMEOUT_SECONDS,
            stdin=None if input_text is not None else subprocess.DEVNULL,
        )
    except (OSError, subprocess.TimeoutExpired) as err:
        raise SnapshotError(f"git {args[0] if args else ''} failed: {err}") from err
    if check and proc.returncode != 0:
        raise SnapshotError(f"git {' '.join(args[:3])} failed: {(proc.stderr or proc.stdout).strip()[:400]}")
    return proc


class GitSnapshotter:
    """Snapshots and restores the files under ``workspace_root`` inside its git work tree."""

    def __init__(self, workspace_root: str, *, log: Optional[Callable[[str], None]] = None) -> None:
        self.workspace_root = str(Path(workspace_root).resolve())
        self.log = log
        if shutil.which("git") is None:
            raise SnapshotError("git is not installed")
        proc = _run_git(["rev-parse", "--show-toplevel", "--git-dir"], self.workspace_root, check=False)
        if proc.returncode != 0:
            raise SnapshotError(f"{self.workspace_root} is not inside a git work tree")
        lines = proc.stdout.splitlines()
        self.top = str(Path(lines[0].strip()).resolve())
        git_dir = Path(lines[1].strip())
        self.git_dir = str(git_dir if git_dir.is_absolute() else (Path(self.workspace_root) / git_dir).resolve())
        rel = os.path.relpath(self.workspace_root, self.top)
        if rel.startswith(".."):
            raise SnapshotError(f"{self.workspace_root} is outside its git work tree {self.top}")
        #: Pathspec (relative to the work tree top) covering the workspace.
        self.scope = "." if rel in (".", "") else rel.replace(os.sep, "/")
        self.last: Optional[Snapshot] = None
        #: Holds the private index path; a finalizer removes the file even if
        #: the caller never calls :meth:`close`.
        self._index_holder: list[str] = []
        self._finalizer = weakref.finalize(self, _unlink_all, self._index_holder)

    # --- helpers ---------------------------------------------------------------------------

    def _env(self, index_file: Optional[str] = None) -> dict[str, str]:
        env = dict(os.environ)
        env["GIT_ATTR_SOURCE"] = EMPTY_TREE
        env.pop("GIT_DIR", None)
        env.pop("GIT_WORK_TREE", None)
        if index_file:
            env["GIT_INDEX_FILE"] = index_file
        else:
            env.pop("GIT_INDEX_FILE", None)
        return env

    def _git(self, args: list[str], *, index_file: Optional[str] = None, **kw: Any) -> subprocess.CompletedProcess[str]:
        return _run_git([*_RAW, *args], self.top, env=self._env(index_file), **kw)

    def _private_index(self) -> str:
        """This snapshotter's own index file (created on first use, removed by :meth:`close`).

        It is never seeded from the real index: entries hashed with the user's
        CRLF/filter settings would be reused through the stat cache and break
        byte-exact restores. Reusing our own raw entries keeps later snapshots
        incremental.
        """
        if not self._index_holder:
            self._index_holder.append(
                os.path.join(self.git_dir, f"bionic-index-{os.getpid()}-{uuid.uuid4().hex[:8]}")
            )
        return self._index_holder[0]

    def close(self) -> None:
        """Remove the private index file."""
        _unlink_all(self._index_holder)

    def _rev(self, rev: str) -> Optional[str]:
        proc = self._git(["rev-parse", "--verify", "--quiet", rev], check=False)
        out = proc.stdout.strip()
        return out if proc.returncode == 0 and out else None

    def _worktree_tree(self) -> str:
        """Hash the workspace into a tree; the private index then lists exactly its files."""
        index = self._private_index()
        # No --ignore-errors: an unreadable file would keep a stale entry and a
        # later restore would overwrite it with old content. Fail instead; the
        # guard then runs the call unprotected or skips the restore.
        proc = self._git(["add", "-A", "--", self.scope], index_file=index, check=False)
        if proc.returncode != 0:
            # Start the next attempt from a clean private index.
            _unlink(index)
            raise SnapshotError(f"git add failed: {(proc.stderr or proc.stdout).strip()[:400]}")
        return self._git(["write-tree"], index_file=index).stdout.strip()

    def _index_tree(self) -> Optional[str]:
        proc = self._git(["write-tree"], check=False)  # fails with unmerged entries
        return proc.stdout.strip() if proc.returncode == 0 else None

    def _commit_tree(self, tree: str, parents: list[str], message: str) -> str:
        args = [*_IDENTITY, "commit-tree", tree]
        for parent in parents:
            args += ["-p", parent]
        return self._git(args, input_text=message + "\n").stdout.strip()

    # --- public API ------------------------------------------------------------------------

    def take(self, label: str = "") -> Snapshot:
        """Capture the workspace before a tool call."""
        tree = self._worktree_tree()
        head = self._rev("HEAD")
        ref_proc = self._git(["symbolic-ref", "-q", "HEAD"], check=False)
        head_ref = ref_proc.stdout.strip() if ref_proc.returncode == 0 else None
        snap_id = uuid.uuid4().hex[:12]
        commit = self._commit_tree(tree, [head] if head else [], f"bionic snapshot {snap_id}: {label}"[:300])
        self._git(["update-ref", "--no-deref", SNAPSHOT_REF, commit], check=False)
        snap = Snapshot(
            id=snap_id,
            tree=tree,
            commit=commit,
            head=head,
            head_ref=head_ref,
            index_tree=self._index_tree(),
            label=label,
        )
        self.last = snap
        return snap

    def changed_since(self, snap: Snapshot) -> list[str]:
        """Paths (relative to the work tree top) that differ from ``snap``."""
        tree = self._worktree_tree()
        if tree == snap.tree:
            return []
        out = self._git(["diff", "--no-renames", "--name-only", "-z", snap.tree, tree]).stdout
        return [p for p in out.split("\0") if p]

    def restore(self, snap: Snapshot, reason: str = "") -> RollbackReport:
        """Put the workspace (and HEAD/index, if they moved) back to ``snap``."""
        report = RollbackReport(restored=False, reason=reason, snapshot=snap.commit)
        tree = self._worktree_tree()
        if tree != snap.tree:
            out = self._git(["diff", "--no-renames", "--name-only", "-z", snap.tree, tree]).stdout
            report.changed = [p for p in out.split("\0") if p]
            # Keep what is about to be thrown away as a stash-shaped commit
            # based on the snapshot (outside the shared stash list), so
            # `git stash show -p <sha>` is exactly the discarded change.
            try:
                base_index = self._commit_tree(snap.tree, [snap.commit], f"bionic snapshot {snap.id} index")
                parents = [snap.commit, base_index]
                saved = self._commit_tree(tree, parents, f"bionic rollback {snap.id}: {reason}"[:300])
                ref = ROLLBACK_REF_PREFIX + snap.id
                self._git(["update-ref", "--no-deref", ref, saved])
                report.saved_ref, report.saved_commit = ref, saved
            except SnapshotError as err:
                report.warnings.append(f"could not save the rolled-back state: {err}")
            # The private index now lists exactly the current files, so
            # restore deletes what the tool created and rewrites the rest.
            self._git(
                ["restore", f"--source={snap.tree}", "--worktree", "--", self.scope],
                index_file=self._private_index(),
            )
            report.restored = True

        head_now = self._rev("HEAD")
        ref_proc = self._git(["symbolic-ref", "-q", "HEAD"], check=False)
        head_ref_now = ref_proc.stdout.strip() if ref_proc.returncode == 0 else None
        if snap.head and head_now != snap.head:
            if head_ref_now != snap.head_ref:
                report.warnings.append(
                    f"HEAD moved from {snap.head_ref or snap.head} to {head_ref_now or head_now}; not switched back"
                )
            else:
                self._git(["update-ref", "-m", f"bionic rollback {snap.id}", "HEAD", snap.head])
                report.head_restored = True
        if snap.index_tree and self._index_tree() != snap.index_tree:
            proc = self._git(["read-tree", snap.index_tree], check=False)
            if proc.returncode == 0:
                report.index_restored = True
                # Refresh stat info with the user's own filters, not the raw ones.
                _run_git(["update-index", "-q", "--refresh"], self.top, check=False)
            else:
                report.warnings.append(f"could not restore the git index: {proc.stderr.strip()[:200]}")
        if report.head_restored or report.index_restored:
            report.restored = True
        if self.log and report.restored:
            self.log(
                f"[rollback] restored workspace to snapshot {snap.id} ({reason}); "
                f"{len(report.changed)} path(s) reverted"
                + (f", discarded state kept at {report.saved_ref}" if report.saved_ref else "")
            )
        return report


def _unlink_all(paths: list[str]) -> None:
    while paths:
        _unlink(paths.pop())


def _unlink(path: str) -> None:
    for candidate in (path, path + ".lock"):
        try:
            os.unlink(candidate)
        except OSError:
            pass


def failure_reason(result: str) -> Optional[str]:
    """Why a tool result counts as a failure (``None`` if it succeeded)."""
    try:
        parsed = json.loads(result)
    except (TypeError, ValueError):
        return None
    if not isinstance(parsed, dict):
        return None
    if parsed.get("denied"):
        return f"denied: {parsed.get('error') or 'tool call denied'}"
    if parsed.get("error"):
        return f"error: {parsed['error']}"
    code = parsed.get("exit_code")
    if isinstance(code, int) and not isinstance(code, bool) and code != 0:
        return f"exit code {code}"
    if parsed.get("ok") is False:
        return "tool reported ok=false"
    return None


def annotate_result(result: str, report: RollbackReport) -> str:
    """Add the rollback outcome to a tool result so the model knows its changes are gone."""
    info = report.as_dict()
    try:
        parsed = json.loads(result)
    except (TypeError, ValueError):
        parsed = None
    if isinstance(parsed, dict):
        parsed["rolled_back"] = report.restored
        parsed["rollback"] = info
        return json.dumps(parsed)
    return json.dumps({"result": result, "rolled_back": report.restored, "rollback": info})


#: ``review(name, arguments, result) -> keep?``; returning False rolls the call back.
ToolReview = Callable[[str, Any, str], bool]


class RollbackGuard:
    """Wraps tool execution: snapshot before, restore on failure or rejection.

    One guard serves a whole run. A workspace that is not a git work tree
    disables it (``available`` is False, ``unavailable_reason`` says why).
    """

    def __init__(
        self,
        workspace_root: str,
        *,
        log: Optional[Callable[[str], None]] = None,
        review: Optional[ToolReview] = None,
        notify: Optional[Callable[[dict[str, Any]], None]] = None,
    ) -> None:
        self.log = log
        self.review = review
        self.notify = notify
        self.snapshotter: Optional[GitSnapshotter] = None
        self.unavailable_reason = ""
        self.rollbacks = 0
        try:
            self.snapshotter = GitSnapshotter(workspace_root, log=log)
        except SnapshotError as err:
            self.unavailable_reason = str(err)
            if log:
                log(f"[rollback] disabled: {err}")

    @property
    def available(self) -> bool:
        return self.snapshotter is not None

    def close(self) -> None:
        if self.snapshotter is not None:
            self.snapshotter.close()

    def run(self, name: str, args: Any, execute: Callable[[Any], str]) -> str:
        """Run ``execute(args)`` under a snapshot; roll back if it fails or review rejects it."""
        if self.snapshotter is None:
            return execute(args)
        try:
            snap = self.snapshotter.take(label=name)
        except SnapshotError as err:
            if self.log:
                self.log(f"[rollback] snapshot before {name} failed, running unprotected: {err}")
            return execute(args)
        if self.notify:
            self.notify({"type": "snapshot", "name": name, "snapshot": snap.commit})
        result = execute(args)
        reason = failure_reason(result)
        if reason is None and self.review is not None:
            try:
                keep = self.review(name, args, result)
            except Exception as err:  # a broken reviewer must not leave a half-applied change
                keep, reason = False, f"review failed: {err}"
            if not keep and reason is None:
                reason = "rejected on review"
        if reason is None:
            return result
        try:
            report = self.snapshotter.restore(snap, reason=f"{name} {reason}"[:300])
        except SnapshotError as err:
            if self.log:
                self.log(f"[rollback] restore after {name} failed: {err}")
            report = RollbackReport(restored=False, reason=reason, snapshot=snap.commit, warnings=[str(err)])
        if report.restored:
            self.rollbacks += 1
        if self.notify and (report.restored or report.warnings):
            self.notify({"type": "rollback", "name": name, **report.as_dict()})
        if not report.restored and not report.warnings:
            return result  # nothing had changed; leave the result alone
        return annotate_result(result, report)
