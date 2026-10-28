-- Each session's colour (a name from c6_api::SESSION_COLOURS), the cue that
-- tells its tiles apart; picked when it starts, changed by you. Empty: none
-- picked (sessions from before colours, which the UI colours by id).
ALTER TABLE sessions ADD COLUMN colour TEXT NOT NULL DEFAULT '';
