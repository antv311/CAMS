# Privacy history cleanup and clone recovery

Task: `task_ac358a96fabf4c649024f27d2f993053`. Owner approval: 2026-11-12,
hub message 126 in `#cams`.

The owner approved replacing the accidental personal Windows path in historical
`src/http.rs` fixtures and force-pushing the rewritten history. The original
literal is deliberately omitted. Contributor names and attribution emails are
preserved. This cleanup also preserves the merged ignore rules and client
template storage work.

## Rewrite procedure

1. Pause project commits, merges, and pushes; record all remote branch and tag
   hashes and local branch/worktree heads. Keep a temporary local recovery
   bundle outside the repository until the replacement has been verified.
2. Prepare an isolated mirror containing all published refs and merged local
   work. Use `git-filter-repo` 2.47.0 with `--sensitive-data-removal`, an external
   `--replace-text` file, `--preserve-commit-hashes`, `--prune-empty never`, and
   `--prune-degenerate never`. The replacement file stays outside Git and is
   removed after verification. Preserve commit messages and history topology.
3. Verify every mapped commit: parent mapping, attribution metadata and message,
   complete file list, and each blob against the original with only the approved
   literal replacement. Scan all rewritten objects and metadata for the original
   path and personal-home variants. Check that current source trees are identical.
4. Run the application suite and redacted Gitleaks history/object scans. Publish
   only verified changed branches using an atomic push with explicit
   `--force-with-lease=<ref>:<recorded-old-hash>` for each ref. Never use a blind
   mirror push that could remove unrelated refs or overwrite concurrent changes.
5. Re-read the remote refs and scan a fresh remote mirror. Update local branches
   and worktree HEAD reflogs to the verified replacement history, expire old
   reflogs, and prune unreachable old objects only after all checks pass. Remove
   temporary copies containing the original data after successful verification.

## Existing clones and unfinished work

The safest recovery is a fresh clone. Save uncommitted work as a patch outside
the clone first. Review saved patches for the personal path before applying them
to the new clone. Do not merge, pull, or push old history into the rewritten
branches, because that would make the original data reachable again.

For unpublished commits, record their purpose and reapply only the reviewed
changes on the rewritten branch. Avoid importing the old branch ancestry.
Coordinate with the project manager before deleting any clone with unique work.

## GitHub and other copies

A force-push replaces branch history; it cannot erase other people's clones or
forks. GitHub may retain old commits in pull request refs or cached commit views.
If those copies expose the path, the owner may need to contact GitHub Support
with the first changed commit and affected pull requests. A clean branch scan
does not prove that every cached copy has been erased.

Reference: [GitHub's sensitive-data removal guidance](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/removing-sensitive-data-from-a-repository).
