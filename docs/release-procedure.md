# Release procedure

This page is the operator-facing checklist for cutting an orno release. The release surface is a supply-chain root: a bad release ships RCE to every CI consumer of `DoctorMozg/orno@v0`. Manual approval gates exist on purpose — do not script around them.

## Prerequisites

- Local clone is on `master`, fully up to date.
- All seven CI gates green on the head commit (`fmt`, `clippy`, `test`, `deny`, `machete`, `typos`, `doc`).
- `CHANGELOG.md` updated with the new version block, including any entries under a `### Security` heading for advisory-grade fixes.
- You have push rights to tags on `DoctorMozg/orno`.

## Cut a release

1. **Fix source first.** No tag goes out unless the commit it points at is independently green. If you spot a regression after tagging, revert the tag — never rewrite history on a published version.
2. **Push the version tag.**
   ```
   git tag v0.1.<n>
   git push origin v0.1.<n>
   ```
   Pre-release? Use `v0.1.<n>-rc.1` (or similar `-` suffix). The release workflow auto-detects pre-releases via `contains(github.ref, '-')`.
3. **Wait for the draft release.** The `release` workflow creates the GitHub Release as a **draft** first, then runs the `upload-assets` matrix. If any platform build fails, `fail-fast: true` aborts the rest and the draft stays in place for you to triage. **Do not retry by re-tagging** — fix the source, delete the draft, retag.
4. **Verify all assets attached.** On the draft release page, confirm:
   - `orno-v0.1.<n>-x86_64-unknown-linux-gnu.tar.gz` (+ `.sha256`)
   - `orno-v0.1.<n>-aarch64-apple-darwin.tar.gz` (+ `.sha256`)
   - `orno-v0.1.<n>-x86_64-pc-windows-msvc.zip` (+ `.sha256`)
   Six files total. Anything missing means the matrix didn't complete — investigate before continuing.
5. **Approve publication.** The `publish-release` job flips `draft: false` automatically once `upload-assets` reports green. There's no manual checkbox today, but the draft window is your verification step. If you spot a problem, delete the draft before the un-draft step lands.

## Move the major tag (`@v0`, `@v1`, …)

The `update-major-tag` job re-points `v<MAJOR>` at the new release, so consumers using `uses: DoctorMozg/orno@v0` follow forward. This runs only after `publish-release` and only on stable tags (the `if: !contains(github.ref, '-')` guard skips pre-releases). The job is serialized via a `concurrency:` group on `github.ref` — two overlapping releases cannot race on the force-push.

If you are cutting a **major bump** (`v1.0.0`), be aware: the workflow will create `v1` automatically. Existing consumers pinned to `@v0` keep getting v0.x patches; they need to opt into v1 explicitly.

## Roll back a bad release

If a published version turns out broken or compromised:

1. **Yank the GitHub Release** — flip it back to draft or delete it. This stops `install.sh`'s "latest release" lookup from finding it.
2. **Rewind the major tag** to the previous good release:
   ```
   git tag -f v0 v0.1.<previous-good>
   git push -f origin v0
   ```
   This is the single point of truth for `uses: DoctorMozg/orno@v0` consumers — once the major tag moves, new CI runs pick up the rollback automatically.
3. **Do not delete the version tag** (`v0.1.<bad>`). Keep the bad tag in git so users referencing it get a clean, identifiable version rather than a confusing 404. Note the rollback in `CHANGELOG.md` under `### Security` if the cause was a vulnerability.
4. **Cut a fixed release** on top of master with the next patch number. Roll the major tag forward to it.

## CHANGELOG security format

Security-relevant fixes go under a `### Security` heading inside the version block:

```
## [0.1.2] - 2026-04-28

### Security
- Fix supply-chain RCE in `action.yml` (would download install.sh from
  attacker-controlled `master` if `ORNO_VERSION` was unset). CVE-pending.

### Fixed
- ...
```

This format is what RustSec and downstream advisory feeds parse. Skipping the `### Security` heading hides the fix from automated tooling even if it lands in the patch notes.

## Why so manual?

Three load-bearing constraints:

- **Major-tag mutation is irreversible-feeling.** Every consumer follows it. A bad force-push affects everyone using `@v<major>` worldwide on their next CI run.
- **Releases ship trust roots.** `install.sh` runs as `bash` on every consumer's runner. The release artifact and its sha256 sidecar are the only things between an attacker and shell on every CI machine that pulls orno.
- **The draft window is a tripwire.** If something automated misbehaves — wrong target, missing checksum, partial matrix — the draft state means nobody downstream ever sees it. Removing the draft step would remove the only stage where a human can intervene cheaply.
