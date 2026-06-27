Prepare to start work: update main, then create a new feature branch or merge main into an existing one.

## Arguments

`$ARGUMENTS` is interpreted as follows. **main is never a valid target** — if the resolved branch would be main, stop and ask for a feature branch name instead.

| Argument | Meaning |
|---|---|
| *(none)* | Create a new branch from latest main. Derive a name from the current task context (e.g. `feature/xxx`). If the work isn't clear, ask first. |
| `current` | Continue on the current branch (HEAD). Must not be main. |
| `feature/xxx` (explicit name) | Target that branch. If it exists, switch to it; if not, create it from latest main. |

## Steps

1. **Check the working tree**
   Run `git status --porcelain`.
   If the output is non-empty, **stop** and ask: "there are uncommitted changes — commit, stash, or discard?" Do not touch the working tree without being told.

2. **Update main**
   Run `git fetch origin main:main`.
   If it fails (non-fast-forward), stop and report — do not force.

3. **Resolve the target branch** from the argument above. Verify it is not main.

4. **Switch / create**
   - New branch (none / explicit-but-missing): `git switch -c <name> main`
   - Existing branch (`current` or explicit-and-exists): `git switch <name>` then `git merge main`

5. **If merge produces conflicts**, stop and report which files conflict. Do not resolve them unilaterally.

6. **Report** the current branch name and status in one line, then say "準備OK".
