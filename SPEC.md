# pmm — Package Manager Manager

One hub — CLI and GUI — for every package manager on a machine: add and remove the
managers themselves, and browse, search, install, update and remove the packages
they hold. Built for keeping track of the chaos without remembering ten different
tools. Intended to be published on GitHub.

Origin: `~/Downloads/pmm-spec.md` (first draft). This file supersedes it.

## Target

- Linux, Debian/Ubuntu-based first (reference: Pop!_OS 24.04, COSMIC, Wayland).
- Rust. MIT license.

## Architecture

A cargo workspace with three crates:

| Crate | What | Distribution |
|-------|------|--------------|
| `pmm-core` | recipes, detection, inventory, cache, all actions | library, crates.io |
| `pmm` | `clap` CLI over pmm-core | crates.io (`cargo install pmm`), GitHub releases |
| `pmm-gui` | libcosmic desktop app over pmm-core | GitHub releases / .deb (not crates.io: libcosmic is a git dependency; not Flatpak: the sandbox can't see host managers) |

All behaviour lives in `pmm-core`; the CLI and GUI only present it. Actions that run
commands report progress as a stream of events, so the CLI can print them and the GUI
can show them live. Swapping GUI toolkit later only replaces `pmm-gui`.

Managers present on the reference machine: apt/dpkg, flatpak, snap, brew, pip, uv,
pipx, npm + npx + pnpm (all under nvm), cargo (via rustup), go (via brew), nix.
Not wanted: gem, yarn.

## Core idea: recipes

Every package manager is a **recipe** — a TOML file describing how pmm deals with it.
Built-in recipes ship inside the binary; user recipes in
`~/.config/pmm/managers/<name>.toml` add new managers or override built-ins.
Adding a manager never needs a code change.

A recipe declares:

| Field | Purpose |
|-------|---------|
| `name`, `description` | identity |
| `detect` | binary name(s) to look for in PATH, or a path to check |
| `list` | command + parser (`lines` with a regex, `json`, or a named built-in parser) |
| `update` | command(s) to update everything; `needs_sudo` flag |
| `install` | one or more named install methods; first is the default |
| `remove` | command(s) to uninstall the manager itself, plus paths it leaves behind |
| `requires` | other managers it depends on (e.g. pnpm → npm, go via brew) |
| `protected` | if true, pmm refuses to remove it (apt, dpkg) |
| `binaries` | the commands a package provides (`glob`, `command` + `pattern`, or `builtin`), for showing which copy of a duplicate runs |
| `search_remote` | search the manager's repository/registry for packages not installed |
| `info` | details for one package (description, version, homepage) |
| `install_pkg`, `remove_pkg`, `upgrade_pkg` | per-package actions, with `needs_sudo` |

Any field can be missing; pmm then just doesn't offer that action for that manager.
Known gap: PyPI has no search API, so pip/uv/pipx support exact-name lookup only.

Some managers need logic a TOML regex can't express (nvm's per-Node-version npm
globals, cargo's `~/.cargo/.crates2.json`). Those recipes point at a named
built-in parser written in Rust.

## Commands

### Managing managers
- `pmm managers` — every known recipe, marked installed / not installed, with how it
  was installed (e.g. `go  installed  via brew`).
- `pmm add <manager> [--via <method>] [--dry-run]` — install a manager using the
  recipe's default method, or the one named with `--via`. Installs any missing
  `requires` first, after asking.
- `pmm remove <manager> [--dry-run] [--yes]` —
  1. Refuse if `protected`.
  2. Show what goes with it: its packages, plus managers that depend on it
     (`remove brew` → go would also go).
  3. Save a manifest of its packages to `~/.local/share/pmm/removed/<manager>-<date>.toml`.
  4. Confirm (skipped with `--yes`).
  5. Remove its packages, then the manager itself.
- `pmm restore <manifest> [--via <manager>]` — reinstall packages from a saved
  manifest, with the same manager or another one.

### Inventory
- `pmm list [--manager <name>]` — counts per manager, or full list for one.
- `pmm search <query> [--installed | --remote] [--manager <name>]` — one search for
  everything. By default: installed matches first (instant, from cache), then
  repository results streamed in per manager as each one answers, marking what's
  already installed. `--installed` stops after the local part; `--remote` skips it.
  Until per-package management lands (build step 6), only the installed part exists.
- `pmm dupes [--all]` — two lists:
  - the same package under more than one manager (names compared
    case-insensitively, `-`/`_`/`.` equal, app part of flatpak ids), with which
    manager's copy each of its commands runs from;
  - the same command in more than one PATH folder, credited to the manager that
    put each copy there, grouped as "brew hides apt: 55 commands" (`--all` lists
    each command and path).
  Copies are credited by the deepest `owns` folder of the file they resolve to,
  a link to a manager's own binary (snap apps), or pip's `RECORD` files (for
  scripts in `~/.local/bin`); otherwise "no manager (folder)".
- `pmm status` — managers detected, package totals, duplicate count, last updated.

### Updating
- `pmm update [--manager <name>] [--dry-run]` — run each manager's update in turn,
  live output, keep going if one fails, summary at the end.

### Managing packages
- Searching repositories is `pmm search` (see Inventory); there is no separate command.
- `pmm info <package> [--manager <name>]` — details from each manager that has it.
- `pmm install <package> --manager <name>` — always explicit about which manager;
  if `--manager` is left out, show the options and ask.
- `pmm uninstall <package> [--manager <name>]` — asks which copy if it's installed
  in more than one place.
- `pmm upgrade <package> [--manager <name>]`.

## GUI (`pmm-gui`)

Same features as the CLI, laid out for browsing:

- **Sidebar**: every manager (installed ones first), with package counts; an
  "Add manager" entry for ones not installed.
- **Installed view**: one searchable, sortable table of everything across managers
  (name, version, manager, last updated), filterable by manager; duplicates flagged.
- **Search bar** (same behaviour as `pmm search`): installed matches first, then
  repository results per manager as they arrive, install button per result;
  toggle for installed-only / remote-only.
- **Package page**: info, which managers have it, install / remove / upgrade.
- **Updates view**: what's outdated per manager, update one / one manager / all.
- **Activity panel**: live output of running commands; sudo prompts go through
  `pkexec` (graphical password prompt).
- Destructive actions (remove a manager, uninstall) show the same "what you'd lose"
  summary as the CLI and need a confirm click.

## Behaviour notes

- **npx** installs nothing; shown as cached tools (`~/.npm/_npx`), not as packages.
- **nvm**: npm/pnpm globals are read for every installed Node version, not just the
  active one.
- **Active in PATH** is decided by binary name, not package name (`ripgrep` → `rg`).
- **Last change** (in `status`) is the newest modification time among a recipe's
  `watch` paths: the last install, removal or upgrade. "unknown" when none exist.
- **Cache**: each manager's list is cached in `~/.cache/pmm/list/`. A recipe's
  `watch` paths (e.g. `/var/lib/dpkg/status`) are fingerprinted by modification
  time; the cache is reused until one changes, for at most 24h (10 min for
  recipes without `watch`). `--refresh` forces a live query.
- **Search** matches names, and `title` (a display name like a flatpak's
  "Firefox"), never `note` (context like "node v22.12.0"). For reverse-DNS ids
  only the last part counts, so `io.github.*` apps don't all match "git".
- pmm never runs `sudo` silently; commands needing it are shown first.

## Build order

1. Recipe format + loader, detection, `pmm managers`.
2. `list` + `search` with cache (apt, cargo, brew, npm first, then the rest).
3. `dupes` + `status`.
4. `update` (with `--dry-run`).
5. `add` / `remove` / `restore` (with `--dry-run`), dependency warnings.
6. Per-package management: remote results in `search`, `info`, `install`,
   `uninstall`, `upgrade`.
7. `pmm-gui`: Installed view first, then Browse, Updates, managers.
8. Publish: README, CI, GitHub releases, crates.io for core + CLI.

## Later

`why`, `cleanup`, `doctor`, `size`, `export`/`import` of the full machine, shell
completions, `config.toml` (ignored managers, preferred manager per package).

## Non-goals (for now)

- macOS/Windows.
- Lockfiles / reproducibility (Nix's job).
