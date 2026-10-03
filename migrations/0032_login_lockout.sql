-- Login lockout: track failed attempts and lock accounts temporarily after
-- too many consecutive failures. Resets on successful login.
ALTER TABLE users
    ADD COLUMN failed_login_attempts INT NOT NULL DEFAULT 0,
    ADD COLUMN locked_until TIMESTAMPTZ;
