# Files named in replies

In a session's agent replies, filesystem paths open the session's worker files
in carbon6, not files on the computer running the browser. This also applies to
side chats and subagent transcripts. Documents, mail and other Markdown without
a session keep their usual rendering.

Absolute paths and `~/…` work in prose or inline code. Relative paths resolve
against the session's working directory (its worktree when known). To avoid
turning ordinary slash-separated words into links, relative paths must start
with `./` or `../`, or have a filename extension and a directory component.
URLs, existing Markdown links and fenced/indented code blocks are not changed.
Put paths containing spaces in backticks. A trailing `:line` or `:line:column`
remains visible but opens the file, without jumping to that line.

A directory opens the file browser. Text opens the existing source/Markdown
viewer; binary formats such as `.docx` offer Download. Missing files show an
explanation. These links use the same realm and worker filesystem access checks
as browsing files, not a new grant to the worker. Links are not generated for
SSH sessions or sessions running on the head: the worker filesystem would be
the wrong machine.

There is no `file://` or “Open on this computer”: HTTPS pages cannot open local
filesystem URLs, and the browser's filesystem usually isn't the worker's.
Revealing a file in Finder would require a separate native integration on that
same machine. Download instead copies the worker's bytes to the browser.

Downloads use the existing bounded worker file-transfer RPC (up to 40 MiB),
with an attachment filename and no browser caching. Larger files are refused,
never silently truncated; publish or split those files with the agent instead.
