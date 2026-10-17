-- Finding mail kept before messages were read once (parsed = 0) without
-- reading every message.
CREATE INDEX mail_parsed ON mail (parsed);
