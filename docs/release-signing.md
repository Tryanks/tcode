# macOS release signing & notarization

The [release workflow](../.github/workflows/release.yml) signs and notarizes
`Tcode.app` and its DMG when all six signing secrets below are configured.
With none configured it publishes unsigned builds; a partial set fails the
packaging step with the missing secret names. Both the ZIP's app and the DMG's
app carry the stapled ticket, and the DMG is separately signed and stapled.

The certificate and notarization credentials must come from the maintainer's
Apple Developer account.

## What you need first

- An **Apple Developer Program** membership.
- A **Developer ID Application** certificate (Xcode → Settings → Accounts →
  Manage Certificates → `+` → *Developer ID Application*, or create it at
  <https://developer.apple.com/account/resources/certificates>).
- Your 10-character **Team ID** (Apple Developer → Membership).

## Secrets to add

Add these under **GitHub → repo → Settings → Secrets and variables → Actions →
New repository secret**:

| Secret | What it is | How to get it |
| --- | --- | --- |
| `MACOS_CERTIFICATE` | base64 of your Developer ID `.p12` | In Keychain Access, **My Certificates** category (not *Certificates* — that omits the private key) → right-click the *Developer ID Application* row → Export → Personal Information Exchange (`.p12`), then `base64 -i cert.p12 \| pbcopy` |
| `MACOS_CERTIFICATE_PASSWORD` | the password you set on that `.p12` export | you chose it during export |
| `MACOS_SIGN_IDENTITY` | the exact identity string | `security find-identity -v -p codesigning` → e.g. `Developer ID Application: Your Name (ABCDE12345)` |
| `MACOS_NOTARY_APPLE_ID` | the Apple ID email for notarization | your developer-account email |
| `MACOS_NOTARY_TEAM_ID` | your 10-char Team ID | Apple Developer → Membership |
| `MACOS_NOTARY_PASSWORD` | an **app-specific password** (not your Apple ID password) | <https://account.apple.com> → Sign-In and Security → App-Specific Passwords → generate one labeled e.g. `tcode-notary` |

## Verify after adding

Cut a prerelease tag (e.g. `v0.0.0-signtest`) and watch the **Sign, notarize, and package macOS** step: it should print `macOS build signed and notarized`.
Download the `.dmg` and the `.zip`, then locally:

```sh
# The DMG container (must pass before you even mount it)
spctl -a -vvv -t open --context context:primary-signature ~/Downloads/tcode-*.dmg
xcrun stapler validate ~/Downloads/tcode-*.dmg
# The app itself (unzip or drag from the DMG to /Applications first)
spctl -a -vvv -t exec /Applications/Tcode.app     # → "accepted  source=Notarized Developer ID"
xcrun stapler validate /Applications/Tcode.app    # → "The validate action worked!"
```

## Not covered

- **Windows code signing** (an OV/EV certificate would remove SmartScreen
  friction) — the workflow ships unsigned Windows zips today.
- **Automatic update installation** — the app checks for releases, but this
  workflow does not provide a Sparkle-style updater.
