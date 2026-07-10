-- Credential lifecycle (increment 0.5): stop storing invitation tokens in
-- plaintext. Invitations get a uuid primary key plus a unique SHA-256 hex
-- token_hash (the same at-rest form as every other token); existing rows are
-- backfilled by hashing the plaintext token, which is then dropped. The
-- accept endpoint hashes the presented token before lookup; creation still
-- returns the plaintext token exactly once.

create extension if not exists pgcrypto;

alter table invitations add column id uuid;
alter table invitations add column token_hash text;

update invitations
set id = gen_random_uuid(),
    token_hash = encode(digest(token, 'sha256'), 'hex');

alter table invitations alter column id set not null;
alter table invitations alter column token_hash set not null;

alter table invitations drop constraint invitations_pkey;
alter table invitations add primary key (id);
alter table invitations add constraint invitations_token_hash_key unique (token_hash);

alter table invitations drop column token;
