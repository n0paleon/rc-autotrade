# AGENTS.md — Instructions for Agents in This Project

This document applies to all agents working in the `rollercoin` repository.
Failure to follow the tool rules below is a procedural violation, not a matter of preference.

## 1. Core Rule: Built-in Tools Always Take Priority

For all file operations — reading, searching, and modifying files — agents **must** use the built-in tools (default tools) first:

| Need | Required built-in tool |
|---|---|
| Read a file or directory | `read` |
| Create or overwrite a new file | `write` |
| Edit a file (text replacement) | `edit` |
| Find files by name or pattern | `glob` |
| Search file contents, strings, or symbols | `grep` |

> Principle: If a task can be done with a built-in tool, it **must** be done with that built-in tool. "Using the shell is faster" is not an acceptable reason to skip it.

## 2. Prohibition: The Shell Is Not a Substitute for Built-in Tools

Agents must not use the shell (`shell` / bash / zsh) or the following utilities as substitutes for built-in tools when reading or editing files:

- `cat`, `head`, `tail`, `less`, `more`
- `grep`, `rg`, `find`, `sed`, `awk`
- `python`, `python3`, `perl`, `node -e`, `bun -e` for reading, writing, or editing files
- `echo`, `printf`, `tee`, `>` / `>>` (redirection) for creating or editing files
- `cp` / `mv` / `rm` for file manipulation that should go through `read` / `write` / `edit`

Examples of violations (not allowed):

```text
# WRONG — use read / grep / edit / write instead
cat src/worker/index.js
grep -rn "token" src/
sed -i 's/foo/bar/g' src/file.js
python3 -c "open('file').write('...')"
echo "..." > AGENTS.md
```

## 3. Exception: Only When Built-in Tools Fail — and Only Temporarily

Using the shell, shell scripts, Python, `sed`, `grep`, or similar non-default tools is permitted **only** under the conditions below, with no other exceptions:

### All of the following conditions must be met

1. **The built-in tool was tried first and demonstrably failed or proved incapable**, for example:
   - `read` / `edit` / `write` / `grep` / `glob` errored, timed out, or the file was too large to be handled by the built-in tool even after trying `offset` / `limit`.
   - The operation is genuinely **not covered** by any built-in tool (binary data, program execution, git, build, test, etc.).
2. **Use is limited strictly to the part that is not covered.** Valid examples:
   - Running a build, test, or dev server: `bun ...`, `npm ...`, `pm2 ...`, `vite ...`
   - Git operations: `git status`, `git diff`, `git log`, etc.
   - Processing binary data or build artifacts that cannot be read as text.
3. **Use is temporary and narrowly scoped.** Once that specific need is fulfilled, the agent **must immediately return** to built-in tools for all subsequent file operations.

### What this exception does not permit

- Using a shell workaround **for the rest of a session** merely because it was needed once.
- Repeatedly using shell `sed` / `python` / `grep` to edit or search files that could have been handled with the `edit` / `grep` tools.
- Skipping the built-in tool attempt out of habit or for the sake of "efficiency".
- Applying the fallback to other files that did not encounter the failure.

> In short: a fallback is a **temporary patch for a single, specific obstacle**, not a change of working mode. Every new file task must start again with the built-in tools.

## 4. Required Procedure When Using a Fallback

When a non-default shell fallback is unavoidable:

1. Briefly state **why** the built-in tool could not be used (what error occurred, or which capability is missing).
2. Limit the command strictly to the file or section that has the problem.
3. Once finished, continue with the built-in tools — do not make the fallback the default pattern.
4. Do not write edited files through the shell while `write` / `edit` are still working.

## 5. What May Still Use the Shell (Not a File Fallback)

The following are **not** file read/edit tasks, so they may use the shell without the conditions above:

- Running project commands: `bun run build`, `bun test`, `bun run dev`, `pm2 ...`, etc.
- Running scripts in `scripts/` or defined in `package.json`.
- Git, process management, and other system commands that have no built-in tool equivalent.
- `ls` / `pwd` only for directory orientation when listing via `read` is impossible — return to `read` immediately for file contents.

## 6. One-Sentence Summary

> **Always start with the built-in tools (`read`, `grep`, `glob`, `edit`, `write`). Shell, scripts, Python, `sed`, and `grep` may be used only temporarily, only for the part that is genuinely not covered by the built-in tools after they have demonstrably failed, and the agent must return to the built-in tools immediately afterward — never make the non-default tool the standard for the rest of the session.**
