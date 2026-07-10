-- Credential lifecycle (increment 0.5): absolute expiry for user tokens.
-- Null means "issued before this migration" and is treated as non-expiring,
-- so existing sessions on the live deployment keep working. Every token
-- issued from now on carries an expiry (default 30 days, configurable via
-- ATLAS_USER_TOKEN_TTL_DAYS). Device tokens deliberately have no expiry —
-- see the token-lifecycle doc-comment in src/auth.rs.

alter table user_tokens add column expires_at timestamptz;
