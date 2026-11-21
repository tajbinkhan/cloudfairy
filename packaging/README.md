# Package manager releases

The [`packages.yml`](../.github/workflows/packages.yml) workflow publishes each GitHub release
to three package managers:

| Package manager | Package | Template |
|---|---|---|
| winget | `TajbinKhan.Cloudfairy` | `winget/*.yaml` (first version only, see below) |
| Homebrew | `tajbinkhan/tap/cloudfairy` in [tajbinkhan/homebrew-tap](https://github.com/tajbinkhan/homebrew-tap) | `homebrew/cloudfairy.rb` |
| AUR | [`cloudfairy-bin`](https://aur.archlinux.org/packages/cloudfairy-bin) | `aur/PKGBUILD` |

[`render.sh`](render.sh) fills the templates for one version. It downloads the release files,
hashes them, and writes the results to `packaging/out/`:

```bash
bash packaging/render.sh 0.1.7
```

The workflow runs when a release is published, but not for pre-releases. Each job is skipped
until its secrets exist, so the workflow is safe to merge before the setup below is done. To
publish a release again, or one whose tag predates the workflow, run **Packages** from the
Actions tab and enter the version.

The AUR package needs a release that has `LICENSE` at its tag, which means v0.1.7 or later.

## One-time setup

### Homebrew

1. Create a public repository named `homebrew-tap` under `tajbinkhan`, with a README so
   it isn't empty.
2. Create a fine-grained personal access token with **Contents: Read and write** on that
   repository only.
3. Save it as the `HOMEBREW_TAP_TOKEN` secret in this repository.

The workflow commits `Casks/cloudfairy.rb` to the tap on each release.

The official `homebrew/cask` repository won't take Cloudfairy until the macOS app is
notarized. Homebrew requires casks to pass Gatekeeper, and it disabled casks that don't on
2026-09-01.

### AUR

1. Create an account on [aur.archlinux.org](https://aur.archlinux.org).
2. Make a new SSH key just for this with `ssh-keygen -t ed25519 -f aur`, and add `aur.pub`
   under **My Account → SSH Public Key**. Don't reuse a key you use anywhere else.
3. Save these secrets in this repository: `AUR_SSH_PRIVATE_KEY` (the contents of `aur`),
   `AUR_USERNAME` and `AUR_EMAIL` (used as the commit author on the AUR).

The first push creates the `cloudfairy-bin` package.

### winget

winget-releaser copies the previous version's manifests, so the first version goes in by hand:

1. Run `bash packaging/render.sh <version>`, then check it with
   `winget validate --manifest packaging/out/winget/manifests/t/TajbinKhan/Cloudfairy/<version>`.
2. Fork [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs), copy
   `packaging/out/winget/manifests/t/TajbinKhan/Cloudfairy/` into the fork's
   `manifests/t/TajbinKhan/`, and open a pull request. A moderator reviews it, which can take
   a few days.
3. After it merges, create a classic personal access token with the `public_repo` scope and
   save it as the `WINGET_TOKEN` secret. The workflow then opens the pull request for every
   later release from your fork. winget-releaser needs a classic token because it opens pull
   requests on microsoft/winget-pkgs, which a fine-grained token scoped to your fork can't do.
   Make it for this alone and give it an expiry date.

The third-party actions in `packages.yml` are pinned to commit SHAs because they receive
these secrets. To update one, look up the new tag's commit and change the SHA and comment.
