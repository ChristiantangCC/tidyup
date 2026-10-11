# tidyup

Free up ports and disk space from your dev projects.

- **Something's already on port 3000?** `tidyup 3000` stops it.
- **Disk full of `node_modules` and `target` folders?** `tidyup clean` finds them and lets you pick what to delete.
- **Starting the same three dev servers every morning?** `tidyup up` starts them all from one file and stops them all with ctrl-c.

Run `tidyup` with no arguments for a dashboard that does both: live ports on one tab, deletable folders on the other.

## Install

```sh
cargo install --git https://github.com/ChristiantangCC/tidyup
```

Works on macOS and Linux. Needs `lsof` and `ps`, which both systems ship with.

## Ports

```sh
tidyup ports          # what's listening, which process, which project folder
tidyup ports --all    # include system processes
tidyup 3000           # stop whatever is on 3000
tidyup kill 3000 5173 --force
```

`ports` hides system processes (anything running from `/`, like AirPlay or Spotify) unless you pass `--all`, so the list shows your dev servers.

Stopping sends SIGTERM and gives the process 3 seconds to exit before sending SIGKILL. `--force` skips straight to SIGKILL.

## Clean

```sh
tidyup clean ~/code                  # pick from a list, then confirm
tidyup clean --older-than 30         # only projects untouched for 30+ days
tidyup clean --dry-run               # list, delete nothing
tidyup clean --yes                   # delete everything found, no questions
```

A folder only counts when the project file that creates it sits next to it. `node_modules` needs a `package.json`, `target` needs a `Cargo.toml` or `pom.xml`, and so on. A `build/` folder you made yourself is never touched.

| Folder | Next to | Kind |
| --- | --- | --- |
| `node_modules`, `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `.parcel-cache`, `.angular` | `package.json` | JavaScript |
| `target` | `Cargo.toml` / `pom.xml` | Rust / Maven |
| `build`, `.gradle` | `build.gradle(.kts)`, `settings.gradle(.kts)` | Gradle |
| `.venv`, `venv` | `pyproject.toml`, `requirements.txt`, `setup.py`, `Pipfile` | Python |
| `Pods` | `Podfile` | CocoaPods |
| `.dart_tool`, `build` | `pubspec.yaml` | Dart / Flutter |
| `vendor` | `composer.json` | PHP |
| `_build`, `deps` | `mix.exs` | Elixir |
| `.zig-cache`, `zig-out` | `build.zig` | Zig |

Hidden folders (`.git`, `.vscode`, `.cargo`, …), `.app` bundles and `~/Library` are skipped. That keeps editor extensions, which ship their own `node_modules`, safe.

Sizes are what deleting actually frees. Files with more than one hard link don't count, because deleting one link frees nothing. So a pnpm project's `node_modules`, which is mostly hard links into the pnpm store, shows up small.

"Age" is the last change to the project folder itself, not to its `node_modules`.

## Up

```sh
tidyup up --init      # writes a starter tidyup.toml
tidyup up             # start every service
tidyup up web api     # start some of them
```

```toml
[[service]]
name = "web"
cmd = "pnpm dev"
port = 3000              # whatever holds this port is stopped first

[[service]]
name = "worker"
cmd = "pnpm worker"
cwd = "packages/worker"  # relative to tidyup.toml
env = { LOG_LEVEL = "debug" }
```

`tidyup up` looks for `tidyup.toml` in the current folder, then in each parent folder. Output from each service is prefixed with its name in its own color. ctrl-c stops every service and anything the services started. Services that are still running after 5 seconds get killed, and a second ctrl-c kills them straight away.

## Dashboard

`tidyup` (or `tidyup ui ~/code`) opens it. `tab` or `1`/`2` switches tabs, `↑↓` or `j`/`k` moves, `q` quits.

| Ports tab | | Clean tab | |
| --- | --- | --- | --- |
| `x` | stop | `space` | mark |
| `X` | force kill | `a` | mark all |
| `a` | show/hide system processes | `d` | delete marked (or current) |
| `r` | refresh (also every 2s) | `s` / `t` | sort by size / age, press again to flip |
| | | `r` | rescan |

## License

MIT
