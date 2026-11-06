# Dictating CV edits over Telegram

The owner sends a Telegram voice note about their CV, and gets a picture of the new CV back. The rule that shapes everything here: **a voice edit can change wording, never a hard fact.** Dates, employers, titles, degrees and numbers belong to the owner, and only the owner on the board can change them.

Nexus holds no Telegram code and no bot token. Hermes does the talking, using the MCP tools.

## Before the first session

The owner does this once, on the board:

1. Get the master profile in with `upsert_cv_profile`.
2. Run `nexus cv-facts set --file facts.json` for what a profile can't express. All fields are optional:

   ```json
   {
     "languages": [{"name": "German", "level": "C2"}],
     "metrics": [{"key": "invoices", "text": "40% fewer invoice errors"}],
     "allowedTokens": ["S3"],
     "bannedPhrases": ["a phrase you never want on your CV"],
     "vocabulary": ["Scrum Master"]
   }
   ```

3. Run `nexus cv-facts show` and read it.
4. Run `nexus cv-facts lock`. It prints every number it took from the profile's wording and added to the allowed tokens. **Read that list: locking confirms each one as true.**

Without a lock, `patch_cv` answers `cv_facts_not_locked`. To change a fact later, run `nexus cv-facts unlock`, change it, and lock again. `set` also works while locked. Every one of these writes a history row.

## The session

1. **Voice note.** The owner speaks. Hermes transcribes it.
2. **Read.** `get_cv_source` returns the source, its `revision`, the facts and help on the template. Facts appear in the source by id, as in `#employment("acme")`, so there is nothing to retype. A variant with nothing stored yet reads as revision 0.
3. **Edit.** `patch_cv` with `expectedRevision` set to that revision, `origin: "telegram"`, a one-sentence `summary`, and `edits`, each an exact `oldText` and its `newText`.
4. **Picture.** On success, `render_cv_pdf` returns a PNG, which Hermes sends back. If `reviewTokens` isn't empty, Hermes also asks the owner to check those words.

`revert_cv` covers "go back to how it was": it adds a new revision holding the old source, so history never shrinks.

## When `patch_cv` says no

Nothing is stored in any of these cases, and the revision stays as it was.

| Code | What Hermes does | Tells the owner? |
|---|---|---|
| `cv_compile_failed` | Reads `diagnostics` (line, column, message, hints), fixes its own source and retries | No |
| `cv_edit_no_match`, `cv_edit_ambiguous` | Fixes the `oldText` that `edit` names, and retries | No |
| `cv_revision_conflict`, `cv_facts_changed` | Calls `get_cv_source` again and reapplies the edit | No |
| `cv_render_busy` | Waits a moment and retries | Only if it keeps happening |
| **`cv_fact_violation`** | **Stops, and tells the owner exactly which `token` was refused** | **Yes, always** |
| `cv_facts_not_locked` | Stops, and asks the owner to run `nexus cv-facts lock` | Yes |

**Never work around a `cv_fact_violation`.** Don't reword the claim, spell the number out, split the word, use lookalike letters, or try another tool. The owner said something the facts don't back. Either the note was misheard, or a fact needs changing, and only the owner can tell which.

A good reply: "I can't say 'eight years': `eight years` isn't in your locked facts. Nothing was changed. If it's true, add it on the board with `nexus cv-facts`."

`upsert_cv_profile` is no way around this. While locked, it answers `cv_facts_locked` to any change of a fact or any new number.

## What the gate can't do

It reads the rendered CV and refuses numbers, dates, spelled-out counts, degree, seniority and language-level words, banned phrases, images and hidden text that the facts don't back. **An invented proper noun in ordinary prose is flagged, not refused**, which is why `reviewTokens` must reach the owner. SPEC section 9a lists the known limits.

## What has been tested

`crates/server/tests/cv_dictation.rs` replays a messy session on invented data, using a scripted client through real OAuth against the real `nexus serve` and the real render child: wording edits, a compile error with its retry, a revert, then voice notes that change a count, a date and a year, inflate a title, claim a degree and use a banned phrase. Each is refused, with the head revision, facts and history unchanged. It ends by decoding the PNG.

**Not tested:** a real Telegram voice note, real transcription and Hermes itself. Those need the owner and the deployed server.
