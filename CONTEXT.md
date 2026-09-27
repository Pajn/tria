# tria

A terminal chat client for T3 Code: a thread picker, a chat view, a composer, and Vim-style
navigation over a running T3 Code server.

## Language

### Reading

**Reading**:
Something read in place of the thread's conversation, which is kept underneath and put back on
the way out. It is either a subagent's transcript or one of the thread's pull requests.
_Avoid_: transcript (for a reading in general), view, overlay

**Transcript**:
A subagent's own conversation, as the provider wrote it to disk. One kind of reading.
_Avoid_: log, output

**Loud read**:
A read someone asked for. Its answer takes over the chat, and its failure is said out loud.
_Avoid_: explicit read, foreground read

**Quiet read**:
A read nobody asked for, made to bring the open reading up to date. It changes only what is still
open, leaves the reader's place alone, and fails silently.
_Avoid_: background read, refresh (on its own)

**Following**:
Keeping the open transcript of a subagent that is still working up to date with quiet reads, until
it finishes.
_Avoid_: polling, live reload

**Stack**:
Pull requests linked to a thread that build one on another, bottom to top. Each one is a **layer**.
_Avoid_: chain
