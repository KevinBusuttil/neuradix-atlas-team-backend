-- Credential lifecycle (increment 0.5): device visibility.
-- `last_seen_at` is stamped on successful device-token authentication,
-- throttled server-side (only written when null or older than 5 minutes) so
-- sync polling does not turn every request into a row write.

alter table devices add column last_seen_at timestamptz;
