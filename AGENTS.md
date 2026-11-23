# Working on edualc as an agent

Claude Code and agy both read this file. Anything decided about how edualc works goes here or in
docs/, never only in one agent's own memory: the other agent can't see it there.

## Checks

- **While you work:** `just feedback <package> [test filter]`, with the package you changed
  (`edualc-core`, `edualc-host` or `edualc`). It formats, then runs ast-grep's rules, clippy and
  that package's tests, one line per finding, and stops at the first stage that fails. A change to
  `edualc-core` needs the loops of `edualc-host` and `edualc` too. `just scan` runs the rules
  alone, without a build.
- **Before you hand in:** `just lint` and `just test` must pass; `just lint-wasm` too after a
  change to `edualc-core` or `edualc`.
- **The rules** are in `lint/rules/`, each with its reason. Fix the code; never add `#[allow]`,
  and don't change a rule or its list of exceptions to make a check pass.
- **Generated files:** after changing `ui/types.slint` or `ui/state.slint`, run `just present`;
  don't edit `crates/core/src/present/model.rs` or `crates/slint/src/ui/model.rs` by hand. After
  changing the command line or `crates/slint/src/start/manual.rs`, run `just manual`.
- **A change to the window is seen running** before it is handed in: say in the commit how
  (a screenshot, a run in a nested window).

## Code

- **Types carry meaning.** Identifiers and quantities are newtypes; a closed set of strings is an
  enum with serde names; a state is an enum, not a set of `bool`s and `Option`s that can combine
  into nonsense.
- **No `Box`, `Arc`, locks or cells.** State has one owner, reached by message.
- **Errors are `thiserror` enums**, a wrapped error a typed `#[source]`, never a string. A
  failure is never turned into a default: no `unwrap_or(Value::Null)`, no empty string for a
  missing argument. Return the `Result`.
- **JSON** that edualc writes or reads is a typed value with serde, the wire's types in
  `crates/core/src/external/`; `json!` is for tests only.
- **The wire:** any change to a type in `crates/core/src/ipc.rs` or what it carries (`Settings`,
  `Change`, …) bumps `PROTOCOL`.
- **`pub` only where another crate uses it.** Tests are written from the contract, without
  sleeps.
- **edualc runs several programs** (Claude Code, agy, opencode). Code that isn't about one of them
  is named for what it does, not for Claude; what differs between them is a `Backend` match or the
  program's own module.
- **Each program behaves as it does without edualc,** as far as it can, and the window shows it
  as the program's own app does. Each reaches the window in its own protocol's terms (Claude
  Code's stream-json, ACP for agy and opencode), never translated into another program's.
  edualc adds only what is its own: the sessions across programs, attention, messages between
  sessions, what a turn changed.
- **No copies of a program's data.** Sessions, transcripts and settings are read where the
  program keeps them (`~/.claude`, agy's `~/.gemini`, opencode's own storage) or asked for through
  its protocol. edualc keeps only what is its own: `config.toml`, `state.json`, `changes/`.

## Commits

- One topic per commit. The title says what changes for you, as the history does; the body says
  why and what was checked. Name the model in a `Co-Authored-By:` line.
- The tree builds at every stop. Work that isn't finished goes on a branch of its own, committed,
  not left uncommitted on main.
- main is pushed only when the user says so. GitHub's main takes signed, linear commits only.
- Edit files with your edit tool, not with scripts that rewrite them.

## Text

- Docs, interface text and comments are plain English to "you". Comments say why, not what the
  code does.
- Write little. No bold-led bullet lists, no image grids, no selling ("powerful", "seamless").
  The README stays short; how a feature works goes in `docs/features.md`.
