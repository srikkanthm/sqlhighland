# Autocomplete intermittently stops working (per tab)

Status: **diagnosed (needs confirmation) — fix not implemented**
Date: 2026-09-13
Area: editor completion (`src/complete/context.rs`, `src/providers.rs`)

## Symptom

Autocomplete stops working for a tab, with no popup at all. Reported
characteristics:

- **No popup at all** (not a wrong/missing-items list).
- The manual trigger (**Ctrl+Space**) does **not** recover it either.
- **Switching to another tab and back does not** recover it.
- Only **creating a new tab** recovers it.
- It is intermittent / not reproducible on demand.

## What was ruled out (and why)

| Suspect | Verdict | Evidence |
|---|---|---|
| Kit sticky `trigger_start_offset` (`gpui-base`) | **Ruled out** | Ctrl+Space calls `completion_items_for` directly (`src/providers.rs::trigger_complete`), bypassing the kit's `handle_completion_trigger` gate entirely, yet it is also dead. |
| Provider weak-handle (`OracleCompleter.view`) dead | **Ruled out** | The manual path doesn't go through the provider at all; it reads the view directly. |
| Preference `complete_auto = false` | **Ruled out** | Only affects the auto-trigger gate, not Ctrl+Space. |
| 2-char gate (`prefix.len() < 2`) | **Ruled out for manual** | `trigger_complete` passes `force = true`, which skips it. |
| Tab-switch state reset | n/a | Switching tabs doesn't clear the per-buffer condition; a fresh tab does. |

The failure is therefore that **`completion_items_for` returns empty for that
tab**, for both the auto and manual paths.

## Root cause (most likely): trivia false-positive in `scan_head`

`completion_items_for` (`src/providers.rs`) returns empty very early when the
caret is considered "in trivia":

```rust
if is_trivia_position(text, offset) {
    return empty;
}
```

`is_trivia_position` (`src/complete/context.rs`) is
`scan_head(&text[..offset]).0` — it scans **all text before the caret** and
reports whether the scan ends inside a string/comment. `scan_head` understands
only:

- single strings `'…'` with `''` escapes,
- line comments `-- …`,
- block comments `/* … */`.

It does **not** understand Oracle alternative quoting (`q'…'` / `nq'…'`), and
it has no tolerance for a stray/odd apostrophe. When any of these appears before
the caret, the scanner believes a string opened and never closed, so
`is_trivia_position` returns `true` for the rest of the buffer and the popup is
dead from that point on.

This explains every symptom: deterministic per buffer, dead for both triggers,
unaffected by tab switching (same buffer), and cured only by a new (empty) tab.

### Confirming test (no code change)

Put the caret at the **very start** of the buffer (Cmd+Up) and press
**Ctrl+Space**. Scanning `text[..0]` is always "code", so suggestions should
appear there (statement keywords) even when the tail is dead. If it is dead
even at offset 0, the cause is elsewhere (see "If not trivia" below).

## Reproducers

An odd/unmatched quote (or an unrecovered `/*`) **before the caret**; put the
caret at the end of the line.

```sql
-- 1. q-quote with an apostrophe inside (definitely unhandled)
SELECT q'[it's]' FROM dual;
SELECT q'!don't!' FROM dual;
SELECT nq'{the user's name}' FROM dual;

-- 2. unescaped apostrophe in a normal string (common typo)
SELECT * FROM emp WHERE ename = 'O'Brien';

-- 3. unclosed block comment
SELECT /* TODO tune this

-- 4. unclosed quoted identifier
SELECT "Emp
```

Should **not** break (negative controls):

```sql
SELECT ename FROM emp;
SELECT 'abc' FROM dual;
SELECT q'[abc]' FROM dual;      -- no apostrophe inside: accidentally fine
SELECT "Emp" FROM emp;          -- closed identifier
-- it's a line comment          -- apostrophe in a line comment
SELECT 'O''Brien' FROM dual;    -- correctly escaped
```

Note: `q'{a''b}'` happens to lex correctly because `''` is read as an escape;
the breaker is an **odd/single** apostrophe inside the q-quote.

## Proposed solution

### 1. Fix the lexer (correctness, required)

Teach `scan_head` (`src/complete/context.rs`) Oracle alternative quoting
(case-insensitive, optional `n` prefix):

- `q'X … X'` / `nq'X … X'` where `X` is `[` `{` `(` `<` (close is the matching
  `]` `}` `)` `>`), or any other single delimiter char (close is the same char).
- Add unit tests next to the existing `trivia_*` tests in
  `src/complete/tests.rs` (`q'[it's]'`, `q'!don't!'`, `q'{a''b}'`, unbalanced
  quotes, unclosed `/*`).

This benefits everything that shares the scanner: `is_trivia_position`,
`word_prefix` (quoted-identifier path), hover, and definition.

### 2. Choose an unterminated-string policy (product decision)

`docs/HISTORY.md` records the original principle: *"wrong-list, never
dead-popup."* Options:

- **(a) Correct-only:** fix `q'…'`; a genuinely unterminated (or typo'd) quote
  still suppresses completion. Minimal, but `'O'Brien'` still dead-tails.
- **(b) Never dead-popup (recommended):** when the scan ends inside trivia,
  don't return empty — fall back to keyword-only (or full) completion so the
  popup never fully dies. Trade-off: suggestions can appear inside an open
  string.
- **(c) Heuristic:** treat an apostrophe flanked by alphanumerics (`O'Brien`)
  as a word char rather than a delimiter. Targeted, not standards-correct.

Recommendation: **(a) + (b)** (or (a) + (c)) — fix q-quotes, and add a fallback
so a malformed quote can't kill completion for the whole buffer.

### 3. Make it diagnosable (optional, cheap)

Instead of silently doing nothing, have `trigger_complete` surface a short
status (e.g. "No suggestions here — inside a string/comment?") when the result
is empty. This turns the next occurrence into an instant diagnosis.

## If it turns out *not* to be trivia

If Ctrl+Space is also empty at offset 0, the empty result is in the candidate
set instead. Check, in order:

1. `detect_join_on` + empty `join_condition_candidates` then
   `CompleteContext::JoinOn` (which pushes nothing) — dead after `JOIN … ON`
   with no FK link.
2. Cache-dependent contexts with a cold/empty cache: `AfterFrom` (tables only),
   `OwnerTables`, `ColumnOf` (unknown qualifier) — no keyword fallback.
3. `rank_candidates` (`src/complete/ranking.rs`) filters by
   `label.contains(prefix)`; a bogus huge prefix (unterminated `"…`) would
   filter everything — but that case is also caught by trivia first.
4. Metadata cache wedged at `loading = true` if a background fetch task was
   dropped (`src/browser.rs::ensure_meta`) — completions degrade to keywords
   normally, so this alone shouldn't cause a fully dead popup.

## Key references

- `src/providers.rs::completion_items_for` (trivia early return) and
  `trigger_complete` (manual path).
- `src/complete/context.rs::scan_head` / `is_trivia_position` / `word_prefix`.
- `src/app/lsp.rs::OracleCompleter` (provider) and
  `is_completion_trigger` (auto gate).
- `src/app/tabs.rs` (provider installation), `src/app/render.rs` (TriggerComplete).
- `src/complete/tests.rs` (`trivia_positions_detected`,
  `trivia_respects_scope_nesting`).
- The kit's sticky `CompletionMenuState.trigger_start_offset` is still not
  cleared by the kit, but the app now **self-heals** it (deferred
  `present_completion_items` when the cursor moves before the offset). That
  wedge produced a *different* symptom — auto-complete dead after
  clearing/replacing the buffer, recoverable with Ctrl+Space — and was fixed on
  2026-09-17; see `docs/AUTOCOMPLETE.md`. It is not this issue (Ctrl+Space is
  also dead here).

> **Status update (2026-09-18):** the trivia false-positive in `scan_head`
> remains the leading hypothesis; the underlying `is_trivia_position` early
> return is still in place (`src/app/lsp.rs`), so this issue stays **open**.
> The scope-aware completion engine later reworked the surrounding context
> detection but did not change the trivia gate.
