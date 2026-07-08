-- Payment plane: tokenized invoice pay links (served under
-- pay.atlas.neuradix.app; the /pay/{token} paths are host-agnostic and live
-- in this same binary). The token is a distinct token kind: its SHA-256 hash
-- lives only here (never in user_tokens/devices/portal_links), so a pay
-- token cannot authenticate any other plane and vice versa.
create table pay_links (
    id         uuid primary key,
    company_id uuid not null references companies (id),
    invoice_id text not null,
    token_hash text not null unique,
    created_by uuid not null references users (id),
    created_at timestamptz not null default now(),
    expires_at timestamptz not null,
    revoked_at timestamptz
);

create index pay_links_company_idx on pay_links (company_id);
