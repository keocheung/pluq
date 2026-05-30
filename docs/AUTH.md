# Authentication & cloud access

pluq reads **public / link-shared** archives with no setup. **Private** files need OAuth.

## Provider support

| Provider | Public link | Private (OAuth) |
|----------|-------------|-----------------|
| Local / HTTP(S) | n/a | n/a |
| Google Drive | ✅ | ✅ `pluq auth add gdrive` |
| Dropbox | ✅ | ✅ `pluq auth add dropbox` |
| OneDrive | planned | planned |

## Authorizing a private provider

```sh
pluq auth add gdrive      # opens a browser, PKCE loopback OAuth flow
pluq auth add dropbox
pluq auth list            # show stored providers + token status (no secrets printed)
pluq auth remove gdrive   # delete a stored token
```

Tokens are stored as JSON in your platform's app data directory, written atomically with
owner-only (`0600`) permissions on Unix. pluq refreshes expired access tokens automatically.

## Terms-of-service scope

pluq is a download/extraction client. **You are responsible for honoring each provider's Terms
of Service and the rights of the file owner.** Accessing **your own** files is fine. For files
shared with you or behind public links, only access content you are permitted to — the
source's ToS and the owner's wishes govern, not pluq.

## Environment variables

`PLUQ_LOG` sets log level (`error`/`warn`/`info`/`debug`/`trace`).

Provider credentials may be supplied at **runtime** via the variables below. Each also has a
compile-time `*_EMBED` twin (read via `option_env!`) that bakes a default into the binary at
build time. Runtime vars take priority over embedded defaults.

| Variable | Purpose |
|----------|---------|
| `PLUQ_GDRIVE_KEY` | Google Drive API key (public-file metadata) |
| `PLUQ_GDRIVE_CLIENT_ID` / `PLUQ_GDRIVE_CLIENT_SECRET` | Google OAuth client (private files) |
| `PLUQ_DROPBOX_APP_KEY` / `PLUQ_DROPBOX_APP_SECRET` | Dropbox app credentials (public shared links) |
| `PLUQ_DROPBOX_CLIENT_ID` / `PLUQ_DROPBOX_CLIENT_SECRET` | Dropbox OAuth client (private files) |

Each variable `X` has a build-time twin `X_EMBED`.

## Zero-config by default, bring-your-own optional

Official pluq release binaries come with built-in app credentials, so **you usually need to
configure nothing**:

- **Public / link-shared** Google Drive and Dropbox archives work out of the box.
- **Private** files need a one-time `pluq auth add <provider>` — a browser consent that grants
  *your* pluq install access to *your* files. pluq stores the resulting token locally
  (owner-only `0600`); it is never shared or embedded. No developer-console setup required.

The credentials built into the binary are **app-identity** credentials (they identify pluq to
the provider), not access to anyone's files. For a command-line app these are not confidential
by design — the OAuth flow is secured by PKCE, not by hiding them. This is the same model `gh`,
`gcloud`, and `rclone` use.

### Bring your own (optional)

The built-in app credentials share a quota across all pluq users. If you hit rate limits, or you
prefer to use your own provider app, set the `PLUQ_*` variables above (e.g. in a `.env`) — they
override the built-in defaults. Then run `pluq auth add <provider>` as usual. Per-provider app
setup steps: TODO (follow-up doc).
