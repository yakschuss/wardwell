# Releasing Wardwell

The owner runs these steps. An agent prepares the release commit but never
tags or pushes a tag.

## 0.12.0

### 1. Check main

```sh
git checkout main
git pull --ff-only
grep '^version' Cargo.toml          # version = "0.12.0"
cargo clippy --lib --bin wardwell -- -D warnings
cargo test
```

### 2. Tag and push

The tag starts `.github/workflows/release.yml`. It builds the macOS arm64 and
Linux x86_64 tarballs, creates the GitHub release, and sends the
`update-formula` dispatch to `yakschuss/homebrew-wardwell`.

```sh
git tag -a v0.12.0 -m "wardwell 0.12.0"
git push origin v0.12.0
gh run watch --repo yakschuss/wardwell "$(gh run list --repo yakschuss/wardwell --workflow release.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
gh release view v0.12.0 --repo yakschuss/wardwell
```

### 3. Check the tap

```sh
gh run list --repo yakschuss/homebrew-wardwell --limit 1
brew update
brew upgrade wardwell
wardwell --version                  # wardwell 0.12.0
```

### 4. Set up this computer

```sh
wardwell setup --dry-run
wardwell setup
wardwell doctor
```

Run `wardwell setup` after every upgrade. The pull service's plist names the
resolved binary path inside the Homebrew Cellar, which changes with each
version. `wardwell doctor` fails the "Tracker pull service" row until setup
rewrites it. Start a new Claude Code session afterwards; running sessions keep
their old hooks.

## Homebrew `service` block (optional)

This is a change to the formula in `yakschuss/homebrew-wardwell`, not to this
repository. Wardwell does not depend on it: `wardwell setup` installs its own
launchd agent, `com.wardwell.tracker-pull`, whenever a tracker binding exists.

```ruby
  service do
    run [opt_bin/"wardwell", "tracker", "pull"]
    run_type :interval
    interval 3600
    log_path var/"log/wardwell-tracker-pull.log"
    error_log_path var/"log/wardwell-tracker-pull.log"
  end
```

`opt_bin` survives upgrades, which the setup agent's Cellar path does not. If
you start it with `brew services start wardwell`, both agents pull. The
per-project tracker lock keeps two pulls from writing at once, but each pull
still calls the provider. Use one. `wardwell setup` installs its own agent again
whenever it is missing, so the setup agent is the one to rely on.
