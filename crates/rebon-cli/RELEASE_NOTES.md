# Rebon CLI release notes

The startup banner shows the section for the running version once, the
first time that version starts. A version without a section shows nothing,
and `scripts/release-cli.sh` refuses to cut a release without one.

Each section is a `## <version>` heading followed by one to four `- ` items.
Keep every item within 36 columns: that is the narrowest the banner's notes
column gets, and a longer line is cut off.

## 1.27.2

- ReadMemory accepts user and index
- Router keeps provider on wake
- Follow-ups survive restarts
- Mid-turn replies stay visible

## 1.27.1

- Router reads project memory first
- It updates notes, not repeats them

## 1.27.0

- SaveMemory is always on hand
- Router keeps lessons per project
- Workers read their project's memory

## 1.26.0

- rebon router plans and runs tasks
- Watchdog catches stuck workers
- Events from any source by thread
- Sessions schedule their own wakeups

## 1.25.2

- Message other sessions with @handle
- Shell calls fold into read/search
- Queued messages show the edit key
- No Worked row while a question waits
