# YouTube credentials (`YOUTUBE_CREDENTIALS`)

The app authenticates to the YouTube API via a single env var, `YOUTUBE_CREDENTIALS`,
set in `.env` at the project root (gitignored — never commit it). It's one line of
JSON packing two things yup_oauth2 needs:

```json
{"client_secret": <content of a Google OAuth "Desktop app" client_secret.json>, "token": <content of a cached youtube_token.json>}
```

`docker compose` picks it up via `environment: - YOUTUBE_CREDENTIALS=${YOUTUBE_CREDENTIALS}`
in `compose.yaml`, which reads `.env` automatically — no file gets mounted into
the container.

## client_secret — static, rarely touched

This identifies the app to Google. It doesn't change on its own; you only
regenerate it if you deliberately rotate it (e.g. it leaked) or start fresh.

1. https://console.cloud.google.com/apis/credentials — pick the right project.
2. Click the OAuth client's name under **OAuth 2.0 Client IDs**.
3. Under **Client secrets**, click **+ ADD SECRET**.
4. Click the download icon (⬇) next to the new secret to get the JSON file.
5. Delete/disable the old secret once the new one works.

## token — the part that actually needs redoing occasionally

This is the refresh token from you (the account owner) approving access once
in a browser. There's no way to get this from Cloud Console or any API call —
it only comes from actually doing the consent flow. Needed again if:

- You've never done it (fresh setup).
- The old one was exposed/compromised.
- The OAuth consent screen is in **Testing** publishing status — tokens issued
  then expire after 7 days regardless of use. Switch it to **Production** in
  Cloud Console (Google Auth Platform → Audience → Publish App) to stop this;
  doesn't require Google's app verification for personal/single-user use, just
  expect an "unverified app" warning during consent (click through it).

### How to actually get one

The code still has the plain file-based OAuth flow built in (`credentials_dir`
path in `pipeline.rs`'s `upload_video`/`add_to_playlist`) — that's the
easiest way to trigger it, no extra flags or manual port forwarding needed:

1. Put your `client_secret.json` at `credentials/client_secret.json`.
2. Make sure a real `.mp4` clip is sitting in `input/`.
3. Run `cargo run --release` **in the VS Code integrated terminal** (not a
   plain SSH shell) — VS Code auto-forwards the local port the OAuth redirect
   needs, so no manual `ssh -L` is required.
4. Wait ~5 minutes for the quiet-period batch trigger. It'll encode, then
   print a Google auth URL.
5. Open that URL, sign in, click through the "unverified app → Advanced → Go
   to [app] (unsafe)" warning if it appears.
6. `credentials/youtube_token.json` gets written automatically once consent
   completes. This does trigger a real (private, unlisted) upload of whatever
   clip you used — that's an unavoidable side effect of this bootstrap path.

### Packing both into `.env`

Once both files exist under `credentials/`, pack them into the single line
(run from the project root):

```bash
printf "YOUTUBE_CREDENTIALS='%s'\n" \
  "$(jq -c -n --slurpfile secret credentials/client_secret.json \
                --slurpfile token credentials/youtube_token.json \
                '{client_secret: $secret[0], token: $token[0]}')" > .env
chmod 600 .env
```

The single quotes around `%s` matter — `.env` parsing strips literal `"`
characters from unquoted values, which mangles the JSON. Single-quoting makes
the whole value literal.

## Gotchas hit while setting this up

- The OOB ("copy-paste a code") OAuth flow yup_oauth2 calls `Interactive` is
  dead — Google disabled it years ago (`Error 400: invalid_request`). Only
  `HTTPRedirect`/`HTTPPortRedirect` work now.
- `docker compose config` prints fully-resolved env values, secrets included —
  don't run it (or anything similar) somewhere it'll get logged/shared.
