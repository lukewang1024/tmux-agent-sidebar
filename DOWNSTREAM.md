# Downstream maintenance

This fork keeps two long-lived branches:

- `main` is a fast-forward mirror of `hiroppy/tmux-agent-sidebar`'s `main`.
- `downstream` is the production branch and contains the local patch stack.

The remotes are named `upstream` for `hiroppy/tmux-agent-sidebar` and `origin`
for `lukewang1024/tmux-agent-sidebar`.

## Sync upstream

```sh
git fetch upstream
git switch main
git merge --ff-only upstream/main
git push origin main

git switch downstream
git rebase main
cargo test
git push --force-with-lease origin downstream
```

Use topic branches for changes that may be submitted upstream. Merge or
cherry-pick the finished change into `downstream`; local workflow-specific
changes do not need to wait for upstream acceptance.

## Release

The checked-out source, update checker, installer, and release assets all point
to this fork. Bump the Cargo version with a `-downstream.N` suffix, update
`Cargo.lock`, then tag the tested `downstream` commit:

```sh
git tag v$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
git push origin downstream --tags
```

The release workflow builds the four platform binaries. Do not publish an
upstream tag from `main` in this fork.
