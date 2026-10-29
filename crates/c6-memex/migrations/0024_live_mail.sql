-- Exact history without reading the rest of the mailbox.
CREATE INDEX mail_live_from ON mail (lower(from_addr)) WHERE draft = 0;

-- The domain after the last @, even with a quoted @ in the local part.
CREATE INDEX mail_live_domain ON mail (
    lower(substr(from_addr, length(rtrim(from_addr, replace(from_addr, '@', ''))) + 1))
) WHERE draft = 0 AND instr(from_addr, '@') > 0;

CREATE INDEX mail_live_sent ON mail (sent) WHERE draft = 0 AND sent = 1;
