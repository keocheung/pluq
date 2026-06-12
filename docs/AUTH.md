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

### Adding cloud files via picker (TUI)

Within the TUI, you can pick files from your cloud storage rather than copying and pasting links:
- **Google Drive private files are picker-only.** pluq uses the **`drive.file`** scope, which grants access only to files you **pick** (or that pluq created). In the TUI, press **`c`** on the Home screen → choose **Google Drive** → a browser **Google Picker** opens → the files you pick are added to **Bookmarks**. Pasting a raw private Drive file-id/URL will *not* open it (only public links work via the API-key path).
- **Dropbox supports paste *or* pick.** You can paste a shared link, or press **`c`** → **Dropbox** to use the **Dropbox Chooser** in the browser.
- The first time you pick from Google Drive, pluq runs the one-time `auth add gdrive` browser consent automatically if you haven't signed in yet.

## Terms-of-service scope

pluq is a download/extraction client. **You are responsible for honoring each provider's Terms
of Service and the rights of the file owner.** Accessing **your own** files is fine. For files
shared with you or behind public links, only access content you are permitted to — the
source's ToS and the owner's wishes govern, not pluq.

## Environment variables

`PLUQ_LOG` sets log level (`error`/`warn`/`info`/`debug`/`trace`).
`PLUQ_OAUTH_PORT` overrides the loopback port used for the OAuth redirect and the TUI picker
pages (default `53682`).

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
prefer to use your own provider app, set the `PLUQ_*` variables above (e.g. in a `.env` or your
shell profile) — they override the built-in defaults. Then run `pluq auth add <provider>` as
usual. If you switch credentials later, `pluq auth remove <provider>` + re-add, so the stored
token matches the new app.

#### Google Drive

1. In the [Google Cloud Console](https://console.cloud.google.com/), create (or pick) a project
   and enable the **Google Drive API** — plus the **Google Picker API** if you want the TUI's
   `c` picker.
2. **API key** (public links + Picker): *APIs & Services → Credentials → Create credentials →
   API key*. Optionally restrict it to the two APIs above. → `PLUQ_GDRIVE_KEY`
3. **OAuth client** (private files): *Create credentials → OAuth client ID → application type
   **Desktop app*** → `PLUQ_GDRIVE_CLIENT_ID` + `PLUQ_GDRIVE_CLIENT_SECRET`. Google issues
   Desktop-app clients a secret and requires it even in the PKCE flow; for an installed app it
   is not treated as confidential. No redirect URI to register — Desktop-app clients accept
   loopback redirects on any port automatically.
4. Configure the **OAuth consent screen**. pluq requests only the non-sensitive
   **`drive.file`** scope (access to files you pick — never your whole Drive), so no Google
   verification review is needed: publish the app to **In production**. If you leave it in
   *Testing*, Google expires refresh tokens after 7 days and you'll re-consent weekly.

> The API key and the OAuth client must belong to the **same project**: the Picker derives the
> project number from the client ID and rejects a developer key from a different project.

#### Dropbox

1. In the [Dropbox App Console](https://www.dropbox.com/developers/apps), create an app:
   **Scoped access** → **Full Dropbox** (an *App folder* app can't read your existing files).
2. *Permissions* tab: check **`files.content.read`** and **`sharing.read`**, then submit.
   Scope changes only affect tokens issued afterwards — re-run `pluq auth add dropbox` if you
   change them.
3. *Settings* tab:
   - **OAuth 2 Redirect URIs** — add exactly `http://localhost:53682/callback`. Dropbox
     requires an exact match including the port; if you set `PLUQ_OAUTH_PORT`, register that
     port instead.
   - **Chooser / Saver domains** — add `localhost` if you want the TUI's `c` picker.
4. A Dropbox app has a single key + secret pair, which fills both variable pairs:

   ```sh
   export PLUQ_DROPBOX_APP_KEY="<app key>"          # public shared links
   export PLUQ_DROPBOX_APP_SECRET="<app secret>"
   export PLUQ_DROPBOX_CLIENT_ID="<app key>"        # OAuth (private files)
   export PLUQ_DROPBOX_CLIENT_SECRET="<app secret>"
   ```

Your app can stay in **Development** status — that's fully functional for your own account.
*Production* only matters if other Dropbox accounts will use your app.
