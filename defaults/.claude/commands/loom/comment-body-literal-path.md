# `--body @path` Does NOT Expand — It Posts the Literal String (canonical)

Single canonical copy; role prompts with a short "`--body @path` Does NOT
Expand" pointer refer here.

**If a comment/review body you're posting (via `gh issue comment`, `gh pr
comment`, or `gh api ... comments`) lives in a scratch/scratchpad file, do not
pass it as `--body @path`.** Unlike some shells' `@file` conventions, `gh pr
comment --body @path` and `gh issue comment --body @path` do **not** read the
file — they post the literal text `@path`. PR #4457 lost an entire
changes-requested review this way (the body was the string
`@/private/tmp/.../review.md`, and the file was later overwritten by another
PR's review). It recurred via `gh api ... -f body=@path` (`-f`/`--raw-field`
never expands `@path`) — see #5252.

```
❌ POSTS THE LITERAL STRING "@path" — NOT THE FILE CONTENTS
   gh pr comment 123 --body @/tmp/review-123.md
   gh pr comment 123 --body "@/tmp/review-123.md"
   gh issue comment 123 --body @/tmp/comment-123.md

❌ ALSO POSTS THE LITERAL STRING — a variable does NOT change what the flag does
   REVIEW_FILE="@/tmp/review-123.md"; gh pr comment 123 --body "$REVIEW_FILE"

❌ ALSO POSTS THE LITERAL STRING — on `gh api`, only -F/--field expands @path
   gh api repos/{owner}/{repo}/issues/123/comments -f body=@/tmp/review-123.md

✅ USE ONE OF THESE INSTEAD
   gh pr comment 123 --body "$(cat <<'EOF'
   ... review prose ...
   EOF
   )"
   gh pr comment 123 --body-file /tmp/review-123.md
   gh api repos/{owner}/{repo}/issues/123/comments -F body=@/tmp/review-123.md
```

Prefer the inline heredoc when the body is short/dynamic; use
`-F/--body-file <path>` when it lives in a file — the one flag on `gh pr
comment`/`gh issue comment` that reads file contents (`gh api ... -F body=@path`
also works; `-f`/`--raw-field` does **not**). **Never** pass `@path` as the
value of `--body`/`-b`. **After posting, re-fetch the comment** (`gh pr view <number>
--comments` / `gh issue view <number> --comments`) to confirm it renders your
prose, not a path string.

### Name every staged body file after its issue/PR number — never a fixed name (#6381)

`/tmp/review-123.md` above is not a stylistic choice — **always suffix a staged
body file with the issue or PR number it belongs to** (`pr-body-<N>.md`,
`review-<N>.md`, `fix-comment-<N>.md`), never a fixed constant like
`pr-body.md` or `review.md`. Wave subagents dispatched by `/loom:sweep` are
one level deep from a single orchestrator session and **share one scratchpad
directory**. Two concurrent agents writing the same fixed path race on it:
`--body-file <path>` can read the *other* agent's body, silently publishing
the wrong title, `Closes #N`, or content (#6381). Two independent
`/loom:sweep` runs on one host collide the same way.

A namespaced path like `/tmp/pr-body-123.md` is still a literal path argument,
so it satisfies the destructive-write guard as before (#4921/#4178); it only
removes the collision.

**A guard denial is not an invitation to re-shape the same value.**
`guard-destructive-generic.sh` hard-denies `--body @path`. On that denial, switch
to `--body-file` or the heredoc — **never** route the same `@path` through a
variable, `--raw-field`, or other wrapper (that evasion recurred on PR #4600,
#4601, and is now denied too).

### Why the heredoc delimiter must be quoted (`<<'EOF'`, not `<<EOF`)

The `<<'EOF'` in the "USE ONE OF THESE INSTEAD" block is load-bearing. An
**unquoted** `<<EOF` expands `$var`, `$(...)` **and backticks**, so prose like
`` `.loom/` `` is *executed* and replaced by its empty output; the error goes to
stderr and the post succeeds with the path deleted (seen curating #9123):

```
❌ cat <<EOF                      → "Sha abc123. The path  is materialized."
   Sha $SHA. The path `.loom/` is materialized.
   EOF
```

- **Rule:** always quote the delimiter for prose bodies — fully literal.
- **The trap:** you want a variable (`$SHA`) in the body. Do not drop the
  quotes to get it; use a placeholder and substitute afterward:

  ```
  ✅ cat <<'EOF' | sed "s/@SHA@/$SHA/g" > /tmp/body-123.md
     Sha @SHA@. The path `.loom/` is materialized.
     EOF
  ```
- **Escaping is not the answer:** one missed `` \` `` fails silently; the quoted
  delimiter is all-or-nothing.
- **Re-read after posting** (see above) — the damage shows only on the forge.
