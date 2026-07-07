# Atlas Team Backend — Deployment Guide (DigitalOcean, Ubuntu 24.04)

Step-by-step deployment of the Atlas Team Rust backend onto the existing
droplet **`ubuntu-btl-portals`**, which already serves
**www.busuttil-technologies.com** through nginx + certbot. The company
website is untouched throughout: nginx routes by `server_name`, so the
backend simply becomes additional virtual hosts alongside the existing
`busuttil-technologies.com` site (and the stock `default` site keeps
catching unknown hosts).

Domain names follow `NEURADIX_DOMAIN_AND_BRAND_ARCHITECTURE.md` (hub repo,
`docs/`): one Axum binary, several logical hostnames CNAMEd onto it, nginx
routing by host — so future service splits are DNS-only changes.

**Live now:** `team.neuradix.app` (workspace/API) and
`sync.atlas.neuradix.app` (sync API — same binary, second identity).
**Reserved (DNS only, no nginx yet):** `pay.`, `connect.`, `portal.`,
`api.`, `files.atlas.neuradix.app` — activated later via §11.

---

## 0. Prerequisites

- Root/sudo SSH access to `ubuntu-btl-portals`.
- Access to the DNS zone for **`neuradix.app`**.
- **RAM check** — the Docker build compiles Rust from source. On a 1 GB
  droplet add swap first or the build will OOM (2 GB+ droplets skip this):

  ```bash
  sudo fallocate -l 2G /swapfile && sudo chmod 600 /swapfile
  sudo mkswap /swapfile && sudo swapon /swapfile
  echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
  ```

---

## 1. Install Docker Engine + Compose plugin

```bash
sudo apt-get update
sudo apt-get install -y ca-certificates curl
sudo install -m 0755 -d /etc/apt/keyrings
sudo curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] \
  https://download.docker.com/linux/ubuntu noble stable" | \
  sudo tee /etc/apt/sources.list.d/docker.list > /dev/null
sudo apt-get update
sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-compose-plugin
sudo systemctl enable docker
```

---

## 2. Clone the backend and set the database password

```bash
cd /opt
sudo git clone https://github.com/KevinBusuttil/neuradix-atlas-team-backend.git
cd neuradix-atlas-team-backend

# Strong Postgres password, kept out of shell history:
echo "POSTGRES_PASSWORD=$(openssl rand -hex 24)" | sudo tee .env
sudo chmod 600 .env
```

Compose picks `.env` up automatically; the backend's `DATABASE_URL` is
templated from it.

**Do not enable the `caddy` service** in `docker-compose.yml` — it is
commented out and must stay that way: nginx owns ports 80/443 on this
server (the company website depends on it).

---

## 3. Bind the backend to localhost only

The compose file publishes `8080:8080` on all interfaces by default. Since
nginx will proxy to it, edit `docker-compose.yml` so the API is never
reachable except through nginx:

```yaml
    ports:
      - "127.0.0.1:8080:8080"
```

(If 8080 is already used on the host, pick e.g. `127.0.0.1:8081:8080` and
adjust the nginx `upstream` in §7 accordingly.)

---

## 4. Build and start

```bash
sudo docker compose up -d --build    # first build takes several minutes (Rust)
sudo docker compose logs -f backend  # wait for: "atlas-team-backend listening on port 8080"
curl -s http://127.0.0.1:8080/health
```

Startup connects to Postgres and applies `migrations/0001_init.sql` +
`0002_postings.sql` automatically — there is no separate schema step.
Both containers are `restart: unless-stopped`, so they survive reboots.

---

## 5. DNS records for `neuradix.app`

In the DNS manager for `neuradix.app`, create:

| Type  | Name (in the `neuradix.app` zone) | Value                  | Status   |
|-------|-----------------------------------|------------------------|----------|
| A     | `team`                            | droplet IP             | live now |
| CNAME | `sync.atlas`                      | `team.neuradix.app.`   | live now |
| CNAME | `pay.atlas`                       | `team.neuradix.app.`   | reserved |
| CNAME | `connect.atlas`                   | `team.neuradix.app.`   | reserved |
| CNAME | `portal.atlas`                    | `team.neuradix.app.`   | reserved |
| CNAME | `api.atlas`                       | `team.neuradix.app.`   | reserved |
| CNAME | `files.atlas`                     | `team.neuradix.app.`   | reserved |

Only `team` carries the IP; everything else chains to it, so a future
server move is a one-record change. Nested names like `sync.atlas` are
ordinary records inside the `neuradix.app` zone — no sub-zone needed.

While in the zone, add the hardening records from the brand doc's
checklist:

- CAA: `0 issue "letsencrypt.org"`
- Since `neuradix.app` sends no mail: `TXT @ "v=spf1 -all"` and a DMARC
  record `_dmarc TXT "v=DMARC1; p=reject"`.

---

## 6. One critical `.app` fact

The entire `.app` TLD is on the browser **HSTS preload list** — browsers
refuse plain HTTP to these hosts. Consequences:

- nothing renders in a browser until certificates are issued (§8);
- every server block must serve HTTPS.

Certbot's HTTP-01 challenge still works (Let's Encrypt is not a browser),
so the flow below is unaffected.

---

## 7. nginx server block

Create `/etc/nginx/sites-available/atlas-team`:

```nginx
# Atlas Team backend — one upstream, multiple logical hosts
# (brand architecture doc §9). Coexists with busuttil-technologies.com:
# nginx routes by server_name, the website block is untouched.
upstream atlas_team_backend {
    server 127.0.0.1:8080;
}

server {
    listen 80;
    listen [::]:80;
    server_name team.neuradix.app sync.atlas.neuradix.app;

    access_log /var/log/nginx/atlas_team.access.log;
    error_log  /var/log/nginx/atlas_team.error.log;

    # Sync payloads carry base64 blobs (attachments).
    client_max_body_size 25m;

    location / {
        proxy_pass http://atlas_team_backend;
        proxy_http_version 1.1;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 60s;
    }
}
```

```bash
sudo ln -s /etc/nginx/sites-available/atlas-team /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx
```

Both hostnames share one block because they are the same binary today.
When a service splits out later (e.g. `connect.` becomes its own webhook
process), it gets its own `server` block with a different `proxy_pass` —
clients and payment providers never see the change. `Host: $host` is
forwarded, so the backend can distinguish logical domains already.

---

## 8. TLS — one SAN certificate for the live names

Certbot is already installed and managing the website's certificate; the
same instance handles the new names (this only **adds** a certificate —
`busuttil-technologies.com`'s cert and its renewals are untouched):

```bash
sudo certbot --nginx -d team.neuradix.app -d sync.atlas.neuradix.app
```

Certbot rewrites the `atlas-team` block for 443 with both names on one
certificate and covers renewal via the existing timer.

**Wildcard alternative:** a `*.atlas.neuradix.app` + `team.neuradix.app`
certificate covers every future Atlas subdomain without re-running
certbot, but requires the DNS-01 challenge
(`python3-certbot-dns-digitalocean` + API token). The SAN approach is
simpler; expanding it later is one command (§11).

---

## 9. Verify, then bootstrap the first company

```bash
curl -s https://team.neuradix.app/health
curl -s https://sync.atlas.neuradix.app/health   # same binary, second identity
# And confirm the website still serves:
curl -sI https://www.busuttil-technologies.com | head -1
```

`POST /companies` is the open bootstrap endpoint — create the company +
owner immediately after going live and **save the returned token**
(everything else requires it):

```bash
curl -s https://team.neuradix.app/companies \
  -H 'Content-Type: application/json' \
  -d '{"name":"Your Business","owner_email":"you@example.com","owner_name":"Kevin"}'
```

Invite further users via `POST /companies/{id}/invitations` (owner/admin
only, 7-day expiry).

Client configuration:

- **Workspace/API base:** `https://team.neuradix.app`
- **Sync base:** `https://sync.atlas.neuradix.app` — configure the logical
  name now so clients never need reconfiguring if sync splits out later.

### Firewall

If ufw is in use, only 22/80/443 should be open — 8080 must **not** be:

```bash
sudo ufw status
# if enabling from scratch:
sudo ufw allow OpenSSH && sudo ufw allow 'Nginx Full' && sudo ufw enable
```

The `127.0.0.1:8080` binding from §3 matters independently of ufw:
Docker's iptables rules normally bypass ufw, so a `0.0.0.0` publish would
be reachable from the internet even with ufw "blocking" it.

---

## 10. Backups — do this before you rely on the system

The repo ships working scripts that run `pg_dump` inside the container:

```bash
cd /opt/neuradix-atlas-team-backend
sudo scripts/backup.sh                 # writes backups/atlas-<timestamp>.sql
```

Schedule nightly (root crontab, `crontab -e`):

```cron
15 2 * * * cd /opt/neuradix-atlas-team-backend && ./scripts/backup.sh >> /var/log/atlas-backup.log 2>&1
```

Practice the restore drill once, into a scratch database — never test on
the live one first:

```bash
sudo scripts/restore.sh backups/atlas-YYYYMMDD-HHMMSS.sql atlas_drill
```

And copy `backups/` off the droplet (DigitalOcean Spaces / `rclone`, or at
minimum droplet snapshots) — a backup on the same disk is not a backup.

---

## 11. Activating a reserved subdomain later (repeatable pattern)

When payment links / connectors / the portal ship, it is three steps and
no client impact:

1. **DNS** — already exists (§5), nothing to do.
2. **nginx** — append the hostname to the shared block's `server_name`,
   or give it its own block if it routes to a different process:

   ```nginx
   server_name team.neuradix.app sync.atlas.neuradix.app pay.atlas.neuradix.app;
   ```

3. **Certificate** — expand in place, then reload:

   ```bash
   sudo certbot --nginx --expand \
     -d team.neuradix.app -d sync.atlas.neuradix.app -d pay.atlas.neuradix.app
   sudo systemctl reload nginx
   ```

Webhook paths are frozen by the brand doc — register these exact URLs
with providers and they will never change:

```text
connect.atlas.neuradix.app/webhooks/stripe
connect.atlas.neuradix.app/webhooks/paypal
connect.atlas.neuradix.app/webhooks/woocommerce
connect.atlas.neuradix.app/webhooks/shopify
```

---

## 12. Updating to a new version

```bash
cd /opt/neuradix-atlas-team-backend
sudo git pull
sudo docker compose up -d --build     # rebuild + restart; new migrations apply on boot
curl -s http://127.0.0.1:8080/health
```

Postgres data lives in the `pgdata` named volume — rebuilds never touch it.

---

## 13. Operations quick reference

| Task | Command |
|---|---|
| Backend logs | `sudo docker compose logs -f backend` |
| Container state | `sudo docker compose ps` |
| Health (local) | `curl -s http://127.0.0.1:8080/health` |
| Health (public) | `curl -s https://team.neuradix.app/health` |
| Restart backend only | `sudo docker compose restart backend` |
| psql shell | `sudo docker compose exec postgres psql -U atlas -d atlas` |
| Manual backup | `sudo scripts/backup.sh` |
| Restore drill | `sudo scripts/restore.sh <file.sql> atlas_drill` |
| Audit feed | `GET /companies/{id}/audit` (owner/admin/accountant token) |

Notes:

- The container runs as an unprivileged user and logs to stdout
  (`RUST_LOG=info` set in compose) — `docker compose logs` is the log
  trail alongside the in-database audit feed.
- Throwaway sandbox: `docker compose run --rm -p 8081:8080 backend --mem`
  gives an in-memory instance that loses everything on exit — never point
  real clients at it.
- When `status.neuradix.io` is stood up, point its uptime monitor at
  `https://team.neuradix.app/health`.
