Compare the current code to the documentation and fix any drift.

## What to check

Read the following source files to understand the current implementation:
- `src/` directory structure (run `find src -name '*.rs' | sort`)
- `Cargo.toml` — `[[bin]]` entries, `[dependencies]`
- `src/routes/` — all route handlers and their HTTP methods/paths
- `migrations/` — current schema files
- `cache/` structure (as described in existing docs; no filesystem scan needed)
- `config.toml.example` — all config keys

Then read the three documentation files:
- `README.md`
- `docs/spec.md`
- `CLAUDE.md`

Run `git diff main...HEAD` (or, if on main, `git diff HEAD~1`) to see what changed recently, and focus on where those changes affect documented facts.

## What to fix

For each of the three docs, fix only the sections that have drifted from reality:
- **README.md**: "ディレクトリ構成" tree, config table, CLI usage examples, feature list
- **CLAUDE.md**: "モジュール構成" tree, route list inside `routes/` entries, cache structure
- **docs/spec.md**: any module descriptions, route tables, or data-flow diagrams that no longer match the code

Rules:
- Touch only the **minimum** set of lines needed to make docs match code
- Preserve the existing writing tone and language (Japanese prose where already Japanese)
- Do **not** rewrite sections that are already accurate
- Do **not** commit — just edit the files and report the diff summary

## After editing

Report a brief summary of what changed in each file (or "no changes needed" if already in sync).
Do not commit or stage the changes.
