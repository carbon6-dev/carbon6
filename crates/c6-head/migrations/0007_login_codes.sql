-- One-time codes, typed in on another device, live beside login links.
ALTER TABLE login_links ADD COLUMN kind TEXT NOT NULL DEFAULT 'link';
