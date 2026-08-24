# macOS release signing & notarization

The release workflow (`.github/workflows/release.yml`) signs and notarizes the
macOS app **only when the six secrets below are present**. Until you add them,
tagged releases keep publishing unsigned builds — nothing breaks, the signing
step just no-ops. Once the secrets exist, every `v*` tag ships a Developer ID
signed, notarized, and stapled `Tcode.app` (in both the `.zip` and `.dmg`), so
users no longer need `xattr -dr com.apple.quarantine`.

Everything here needs your Apple Developer account; it cannot be automated.

## What you need first

- An **Apple Developer Program** membership ($99/yr).
- A **Developer ID Application** certificate (Xcode → Settings → Accounts →
  Manage Certificates → `+` → *Developer ID Application*, or create it at
  <https://developer.apple.com/account/resources/certificates>).
- Your 10-character **Team ID** (Apple Developer → Membership).

## Secrets to add

Add these under **GitHub → repo → Settings → Secrets and variables → Actions →
New repository secret**:

| Secret | What it is | How to get it |
| --- | --- | --- |
| `MACOS_CERTIFICATE` | base64 of your Developer ID `.p12` | Export the cert **with its private key** from Keychain Access as `cert.p12`, then `base64 -i cert.p12 \| pbcopy` |
| `MACOS_CERTIFICATE_PASSWORD` | the password you set on that `.p12` export | you chose it during export |
| `MACOS_SIGN_IDENTITY` | the exact identity string | `security find-identity -v -p codesigning` → e.g. `Developer ID Application: Your Name (ABCDE12345)` |
| `MACOS_NOTARY_APPLE_ID` | the Apple ID email for notarization | your developer-account email |
| `MACOS_NOTARY_TEAM_ID` | your 10-char Team ID | Apple Developer → Membership |
| `MACOS_NOTARY_PASSWORD` | an **app-specific password** (not your Apple ID password) | <https://account.apple.com> → Sign-In and Security → App-Specific Passwords → generate one labeled e.g. `tcode-notary` |

## Verify after adding

Cut a prerelease tag (e.g. `v0.0.0-signtest`) and watch the **Sign and
notarize macOS app** step: it should print `macOS build signed and notarized`.
Download the `.dmg`, then locally:

```sh
spctl -a -vvv -t install /Applications/Tcode.app   # → "accepted, source=Notarized Developer ID"
xcrun stapler validate /Applications/Tcode.app      # → "The validate action worked!"
```

## Not covered

- **Windows code signing** (an OV/EV certificate would remove SmartScreen
  friction) — the workflow ships unsigned Windows zips today.
- **A self-updating installer** (Sparkle appcast, etc.) — deliberately deferred
  until signed builds exist; the in-app update-available check is already live.
