# Dictating CV edits over Telegram

The owner sends a Telegram voice note about their CV, and gets a picture of the new draft back. The rule that shapes everything here: **an agent can change wording freely, and nothing it writes is final until the owner has seen it and approved exactly that text.** Dates, employers, titles and degrees in the facts belong to the owner, and only the owner can change them.

Nexus holds no Telegram code and no bot token. Hermes does the talking, using the MCP tools. **Hermes can't approve anything: no tool can.**

## Before the first session

The owner does this once, on the board. Each command runs as the service user, as in `sudo -u nexus nexus cv-facts show --data-dir /var/lib/nexus`, so that user must be able to read `facts.json`. It is safe while the server runs. All of it also works on the dashboard's Facts page, which shows the same list before a lock.

1. Get the master profile in with `upsert_cv_profile`.
2. Run `nexus cv-facts set --file facts.json` for what a profile can't express. All fields are optional:

   ```json
   {
     "languages": [{"name": "German", "level": "C2"}],
     "metrics": [{"key": "invoices", "text": "40% fewer invoice errors"}],
     "allowedTokens": ["docs.example.org/guide"],
     "bannedPhrases": ["a phrase you never want on your CV"],
     "vocabulary": ["Scrum Master"]
   }
   ```

   `allowedTokens` are the URLs and emails a CV may show beyond your contact details. `vocabulary` is words review needn't list.

3. Run `nexus cv-facts show` and read it.
4. Run `nexus cv-facts lock`. It prints every fact and every piece of wording that differs from the last lock, old and new, and every URL it would take from your wording. If there is any, it writes nothing, exits non-zero and names a hash. **Read the list: locking confirms all of it as true, anything an agent wrote is in it, and a CV generated from that wording needs no further approval.** If it's right, run `nexus cv-facts lock --confirm <hash>`. If anything was written after the printout, the hash no longer matches: it prints the new list and refuses.

Without a lock, `patch_cv` and `revert_cv` answer `cv_facts_not_locked`, and so does `render_cv_pdf` when asked to save to an application. To change a fact later, run `nexus cv-facts unlock`, change it through `upsert_cv_profile` or `set`, and lock again. `set` also works while locked. Every one of these writes a history row, and so does every `upsert_cv_profile` that changes a fact while unlocked, as `changed_by: mcp`.

A changed fact makes everything built on it a draft again. An unlock and relock with no change, or a vocabulary edit, doesn't.

## The session

1. **Voice note.** The owner speaks. Hermes transcribes it.
2. **Read.** `get_cv_source` returns the source, its `revision`, its `state`, the facts and help on the template. Facts appear in the source by id, as in `#employment("acme")`, so there is nothing to retype. A variant with nothing stored yet reads as revision 0.
3. **Edit.** `patch_cv` with `expectedRevision` set to that revision, `origin: "telegram"`, a one-sentence `summary`, and `edits`, each an exact `oldText` and its `newText`. The reply says `state: "draft"`, the `reviewItems` and an `approvalPath`.
4. **Picture.** `render_cv_pdf` returns a PNG with `DRAFT, NOT APPROVED` across it, and no PDF. Hermes sends the PNG, then the most important review items in words, then the approval path on the owner's dashboard.
5. **Approval is the owner's.** The owner opens that path, reads the stamped picture and the rendered text against what they last approved, and taps Approve. Or, on the board, `nexus cv-facts approve --variant default --revision N` prints the same, and `--confirm` approves it.

**Hermes may never call the CV final, ready or approved until `state` is `approved`.** Saying "I've updated your draft, please check it on your dashboard" is true. Saying "your CV is updated" isn't, until then.

A good reply: "Saved as a draft. Please check these before approving: `8 years` is a new number here, and `Stanford` is a new word. Approve it at https://your-dashboard/cv/variants/default#cv-approval, and only then is there a PDF."

`revert_cv` covers "go back to how it was": it adds a new revision holding the old source, so history never shrinks. Undoing drafts back to the newest text the owner approved is approved at once, so an undo needs no tap. Text the owner has since replaced stays replaced: a revert to it is a draft, and the review shows what it undoes. So does naming its old revision to `render_cv_pdf`: no PDF, and no document. Only the owner approving it again brings it back.

## When `patch_cv` says no

Nothing is stored in any of these cases, and the revision stays as it was.

| Code | What Hermes does | Tells the owner? |
|---|---|---|
| `cv_compile_failed` | Reads `diagnostics` (line, column, message, hints), fixes its own source and retries | No |
| `cv_edit_no_match`, `cv_edit_ambiguous` | Fixes the `oldText` that `edit` names, and retries | No |
| `cv_revision_conflict`, `cv_facts_changed` | Calls `get_cv_source` again and reapplies the edit | No |
| `cv_render_busy` | Waits 10 seconds and retries, at most 3 times. A compile takes seconds, and retrying faster only keeps the queue full | Only if it keeps happening |
| **`cv_fact_violation`** | **Stops, and tells the owner exactly which `token` was refused** | **Yes, always** |
| `cv_facts_not_locked` | Stops, and asks the owner to run `nexus cv-facts lock` | Yes |

**`cv_fact_violation` is now narrow, and it is final.** It no longer means "a number I don't know": those are review items. It means a reader of the PDF would see something other than what the owner reviews, or the owner's own banned phrase: hidden or covered text, text too pale to read on what it stands on, a letter the CV's font doesn't have (Chinese, Arabic, Devanagari, or any letter in math), rotated or mirrored text, an image, an email, URL or phone number the facts don't back, mixed-script words, letters that read as digits. **Never work around one.** Don't reword it, split it, use lookalike letters, or try another tool. A list stops at 50, and `truncated` says how many more there were.

## When a document is asked for

`generate_tailored_cv` and `render_cv_pdf` with `saveToApplicationId` store a document only for text the owner approved. Otherwise they answer `{"error":{"code":"cv_approval_required","variant":...,"revision":...,"approvalPath":...}}`, having stored the revision and no document. Hermes tells the owner the path and stops. Once the owner has approved it, the same call goes through, and there is no second approval: text identical to the newest approved text of that variant is already approved.

A generate from the owner's own wording, unchanged since the last lock and with no `profileOverride`, needs no tap if it is a plain selection of the locked jobs, in any order: the lock approved it. A `profileOverride`, wording changed since the lock in any way, a regroup or a changed tier included, a job or category named twice, no job at all, or a variant where the owner has since approved something else, is a draft like any other. So is wording the owner has replaced, even if an agent writes it back.

**A submission must only use a document made from an approved revision.**

## When the answer never comes

A lost connection stops a compile that is still running, and then nothing is stored. It can't stop what comes after the compile, and a proxy in between may not pass the disconnect on. So **a call that got no answer may have stored its revision.** Don't assume either way: call `get_cv_source`. If `revision` moved and the source holds the edit, it went through. If not, apply it again with the `expectedRevision` that call returned. Retrying blindly is safe too, since an old `expectedRevision` gets `cv_revision_conflict`.

`upsert_cv_profile` changes wording while locked, numbers included, and answers `cv_facts_locked` to any change of a fact. A wording change is no way around any of this: a CV generated from it is a draft.

## What the gate can't do

It can't tell a true sentence from a false one. No list of words can, and that is why the owner reads the text. The owner can approve something false, and that is the point of it being the owner. `reviewItems` are highlights: every number and date on a changed line, degree, seniority and language words, a company shown outside its own entry or near another's date, title or place, a language level that differs from the facts, a fact that went missing, and new words. A false sentence with none of those shows only as a changed line. SPEC section 9a lists the known limits.

## What has been tested

`crates/server/tests/cv_gate.rs` takes each false fact from the independent review through real OAuth, the real `nexus serve`, the real render child and the real `nexus cv-facts approve`: draft, tokens listed, no PDF, a stamp in the pixels, no document, and after approval a real PDF and no stamp. It also runs a chain of three drafts against the last approved text, and every listed tool with attack arguments.

`crates/server/tests/cv_dictation.rs` replays a messy session on invented data, using a scripted client the same way: wording edits, a compile error with its retry, a revert, voice notes that change a count, a date and a year, inflate a title or claim a degree, each stored as a draft with its token listed, and a banned phrase refused. It ends by decoding the stamped PNG, approving on the board and reading the real PDF.

`crates/server/tests/cv_review.rs` runs the real server and the real `nexus cv-facts` for what an owner and a crowding or lost client see: a lock showing what an agent wrote, a lock confirmed with an old hash, a crowd of compiles refused at once, and a client that disconnects during a compile.

**Not tested:** a real Telegram voice note, real transcription and Hermes itself. Those need the owner and the deployed server.
