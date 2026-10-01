# Releasing Wardwell

The owner runs these steps. An agent prepares the release commit but never
tags or pushes a tag.

## 0.13.3

The steps are the ones for 0.13.0 below, with `0.13.3` in place of `0.13.0`.

0.13.3 changes the Linear gate to ruleset `linear-updates` version 3. A
created issue needs a parent issue, a project, or a template.

## 0.13.2

The steps are the ones for 0.13.0 below, with `0.13.2` in place of `0.13.0`.

0.13.2 changes the Linear gate to ruleset `linear-updates` version 2. A
session may now set an existing issue to `Done`, and only `Done`.

## 0.13.1

The steps are the ones for 0.13.0 below, with `0.13.1` in place of `0.13.0`.

0.13.1 drops the background pull service. Session start and the running
server now refresh the tracker mirror when it is over an hour old. After the
upgrade, run `wardwell setup`. When your vault is under iCloud Drive,
`~/Documents`, `~/Desktop` or `~/Downloads`, the plan shows REMOVE + BACKUP for
the old `com.wardwell.tracker-pull` agent. The old agent waits on a macOS
privacy prompt after every upgrade, so it is the reason a mirror went stale
without a word.

Then check the words:

```sh
wardwell tracker status
wardwell doctor
```

A stale mirror says so and says why. Start a Claude Code session in a mapped
project. When the mirror is over an hour old, the session prints "Refresh
started in the background." `wardwell tracker status` then shows `pull
running since <time>`, and later a fresh last pull.

Sessions and servers started before the upgrade keep the old binary. The old
binary skips `pull_started` rows and `timeout` and `spawn` failures as
unreadable lines. It does not start background pulls. Restart them.

## No Homebrew `service` block

Do not add a `service` block to the formula in `yakschuss/homebrew-wardwell`,
and do not run `brew services start wardwell`. A Homebrew service is a launchd
agent. It waits on the same macOS privacy prompt after every upgrade, when
the vault is under a protected folder. The session refresh needs no prompt.
If an earlier note had you add the block, remove it from the formula. If you
started the service, stop it:

```sh
brew services stop wardwell
```

## 0.13.0

### 1. Check main

```sh
git checkout main
git pull --ff-only
grep '^version' Cargo.toml          # version = "0.13.0"
cargo clippy --lib --bin wardwell -- -D warnings
cargo test
```

### 2. Tag and push

The tag starts `.github/workflows/release.yml`. It builds the macOS arm64 and
Linux x86_64 tarballs, creates the GitHub release, and sends the
`update-formula` dispatch to `yakschuss/homebrew-wardwell`.

```sh
git tag -a v0.13.0 -m "wardwell 0.13.0"
git push origin v0.13.0
gh run watch --repo yakschuss/wardwell "$(gh run list --repo yakschuss/wardwell --workflow release.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
gh release view v0.13.0 --repo yakschuss/wardwell
```

### 3. Check the tap

```sh
gh run list --repo yakschuss/homebrew-wardwell --limit 1
brew update
brew upgrade wardwell
wardwell --version                  # wardwell 0.13.0
```

### 4. Set up this computer

```sh
wardwell setup --dry-run
wardwell setup
wardwell doctor
```

0.13.0 adds the github binding. To use it, add a `provider: github` entry with
`repository: <owner>/<name>` to the project's `trackers` list. When `gh` is
installed and signed in, nothing else is needed. Otherwise store a token:

```sh
pbpaste | wardwell tracker connect github --token-stdin
wardwell tracker doctor
wardwell tracker pull --project <domain>/<project>
```

The first github pull takes the 200 most recently updated merged pull
requests. Run `wardwell tracker pull --project <domain>/<project> --full` once
for the whole history. An edit to a pull request after merge is picked up by
the next pull.

Sessions started before the upgrade keep the old binary until they restart.
The old binary does not know about markers from more than one provider. It
can read a github marker as Linear's. It then shows a wrong Linear pull time,
and its on-miss refresh can start from the wrong point. Restart those
sessions. The daily full Linear pull repairs anything the old binary missed.

The hooks name the path you ran `wardwell` from, such as
`/opt/homebrew/bin/wardwell`, never the versioned Cellar path behind it, so
they survive `brew upgrade`. Start a new Claude Code session afterwards;
running sessions keep their old hooks.

