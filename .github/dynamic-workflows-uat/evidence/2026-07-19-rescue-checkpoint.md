# Dynamic Workflows pre-control rescue checkpoint — 2026-07-19

## Result

The exact active dirty tree was captured without staging anything in the real
index. All repository-writing workers paused for the capture, the porcelain
status was byte-identical before and after the alternate-index population, and
the alternate index had neither worktree differences nor remaining untracked
paths.

## Identity

- Repository: current repository root (`./`)
- Branch: `claude/dynamic-workflows-impl`
- Parent HEAD: `561b4dadee915285adc11efd2e9f6296c83989fc`
- Dirty status entries: 287
- Rescue ref:
  `refs/rescue/dynamic-workflows/checkpoint/10-active-tree-20260719T073228Z`
- Checkpoint commit: `736c063d606295e41eca476fc5ffdb342fd03248`
- Checkpoint tree: `99cc6fbf90001eb689fc43f0cffcd4021ef5f6cb`

## Bundle

- Local bundle:
  `.git/rescue-bundles/dynamic-workflows-active-tree-20260719T073228Z.bundle`
- SHA-256:
  `4128079e8719a504833f82b92dff7e5ca7ea5823bde7d5c429f14c3108a4d118`
- `git bundle verify` reports the bundle is valid, contains the rescue ref, and
  records complete history.
- `git bundle list-heads` resolves the ref to the checkpoint commit above.

## Real-index integrity

The real `.git/index` SHA-256 was identical before and after capture:

`bbfb48769cf72aecd33e1805ba1afc2d85944613b8592244dae65a20ede847d0`

The alternate index wrote tree `99cc6fbf...`, exactly matching the rescue
commit's tree, and `git ls-files --others --exclude-standard` under that index
returned no paths. The disposable alternate-index directory was removed only
after these checks passed.

This is an immutable pre-control-stage recovery point, not the final delivery
snapshot. Subsequent implementation requires a new exact checkpoint and bundle.
