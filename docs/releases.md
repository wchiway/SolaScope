# Release automation

A push of an existing version tag such as `v0.1.1` runs `.github/workflows/release.yml`. Successful validation, packaging, and description generation publish a GitHub Release automatically. Commit-message-based draft creation and cross-run CI artifact selection are no longer used.

## Repository configuration

For an AI-generated summary, configure these in the repository's **Settings → Secrets and variables → Actions**:

| Setting | Kind | Purpose |
| --- | --- | --- |
| `DEEPSEEK_API_KEY` | Repository secret | Bearer credential for the DeepSeek Responses API |
| `DEEPSEEK_MODEL` | Repository variable, optional | Defaults to `deepseek-flash`; `deepseek-v4-pro` is another documented option |

Do not commit credentials, put keys in workflow YAML, or include them in release text. Only the notes-generation step receives the DeepSeek secret; test/build/collection jobs do not inherit it. Only the final publishing job has `contents: write`. GitHub's generated `GITHUB_TOKEN` is used for GitHub operations.

The integration calls **`POST https://api.deepseek.com/responses`**, not Chat Completions. It uses Python's standard library, non-streaming output, `reasoning.effort = none`, and bounded output. Completed assistant message `output_text` content is published; reasoning content is ignored. DeepSeek's Responses API is stateless and reports `store: false`; the client does not send the unsupported `store` parameter. See the [official API contract](https://api-docs.deepseek.com/api/create-response/) and [Responses guide](https://api-docs.deepseek.com/guides/responses_api/).

If the key is absent, the provider rejects the request, the connection fails, or output is invalid/incomplete, the workflow emits a warning and keeps the factual description. A provider outage does not block an otherwise validated release. Invalid release inputs and package-validation failures still block publication.

## Publishing a version

1. Update `workspace.package.version` in `Cargo.toml` and applicable public documentation. Review and commit the changes, and ensure the commit is in remote `main` history.
2. Configure the optional DeepSeek secret before releasing if an AI summary is wanted.
3. Create a lightweight or annotated tag exactly matching the workspace version, then push that tag:

```bash
git tag v0.1.1
git push origin v0.1.1
```

These are maintainer operations, not performed by the installer or local validation. Replace `v0.1.1` with the intended version. Do not move or replace an existing published tag.

4. Watch the **Release** workflow. It performs the following:
   - Checks the tag is `v<SemVer>`, resolves annotated tags to their commit, matches the triggering checkout, and confirms the commit belongs to `origin/main` history.
   - Runs the reusable **Test and package** workflow on the tag's exact commit: three-platform validation, optional Linux local-reranker tests, and three platform builds.
   - Downloads only the packages produced in this same run, verifies archive contents and hashes, and exports standalone installers from the validated archives. The Linux and macOS copies of `install.sh` must be byte-identical.
   - Builds an author-free bounded changelog; optionally sends it to DeepSeek for a labeled summary, then appends the factual commit list and distribution information.
   - Rechecks the remote tag immediately before release creation, rejects an existing release, creates a temporary draft, uploads every asset, verifies asset names/sizes/SHA-256 digests, and publishes only after all uploads succeed.

There are seven assets: three platform archives, `install.sh`, `install.ps1`, `install.cmd`, and `SHA256SUMS`. The checksum file covers the other six assets. Upload verification fails closed if GitHub reports a missing/mismatched asset digest, an unexpected filename, an incorrect size, or an incomplete upload; the release remains a draft for inspection rather than being published. Windows x64 release builds use static CRT linkage and inspect ordinary and delay-loaded PE imports for non-system dependencies. The Windows installer resources are also inside the Windows ZIP, and `install.sh` is inside each Unix archive; public installation/release guides are packaged on all platforms.

SemVer prerelease versions, such as `v0.2.0-rc.1`, publish as prereleases and are not selected by the installer’s stable/latest lookup. Stable releases use GitHub's legacy date/version selection for latest, rather than forcibly marking an older maintenance release as latest.

## Retrying failures

You may rerun a failed workflow from Actions, or dispatch it on an existing tag:

```bash
gh workflow run release.yml --ref v0.1.1
```

Dispatching on a branch is rejected. Retry runs validate and rebuild that tag instead of trusting an arbitrary past run ID. Existing releases, including drafts left by interrupted uploads, are never overwritten automatically. Inspect a partial draft and its assets before deciding how to recover; the workflow does not delete assets, delete releases, move tags, or rewrite history on your behalf.

## AI input and output boundaries

Only the selected version, previous reachable version tag, commit count, and up to 100 short commit IDs/subjects (240 characters per subject) are supplied to DeepSeek. The first release uses the bounded repository history; later releases use the range after the closest previous reachable valid version tag on a different commit. Author identities, repository identity, source files/diffs, environment values, and credentials are not supplied. Obvious emails, mentions, URLs, and common token prefixes are removed from subjects. Commit subjects are intended public release metadata; redaction cannot guarantee that an arbitrary subject never contains sensitive material. Review commit subjects before releasing.

The prompt treats commit text as untrusted data. The client rejects redirects, maintains normal TLS verification, limits request/response sizes, applies per-request timeouts, and retries transient HTTP failures at most once. Raw provider error bodies and credentials are not logged. Generated summaries containing links, mentions, HTML, code blocks, or invalid controls are discarded.

AI summarization is not an independent factual check: it can misunderstand a subject. The output is labeled and the factual changelog is retained. It must not be treated as proof of live-model quality, platform compatibility, or successful clean-machine execution. No source/model configuration is changed by release-note generation.

## Local validation

```bash
python -I -B -m unittest discover -s .github/scripts -p 'test_*.py' -v
actionlint .github/workflows/ci.yml .github/workflows/package.yml .github/workflows/release.yml
```

On Windows, deterministic installer fixtures use the built-in Windows PowerShell 5.1 and no Pester or package manager:

```powershell
powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass `
    -File .github/scripts/test_windows_install.ps1
```

On Linux/macOS, check the script and optionally test an existing native package:

```sh
sh -n install.sh
shellcheck install.sh
shfmt -i 4 -d install.sh
python -I -B .github/scripts/test_unix_install.py --real-archive /path/to/solascope-VERSION-TARGET.tar.gz /path/to/SHA256SUMS
```

ShellCheck and shfmt are developer checks, not installation prerequisites. The packaging workflow passes its real native archive and checksum to the Unix or Windows installer test runner for repeated offline-install smoke checks. Tests isolate filesystem/configuration/environment state and do not write the real User PATH or call GitHub/DeepSeek. Unix fixtures cover archive rejection, download fallback, architecture selection, upgrades/rollback, version-output checks/timeouts, and timer cleanup. Python tests also cover Responses parsing/fallback/errors, version-tag validation, author-free facts, ordinary/delay PE imports, identical Unix installer resources, and archive/installer checksum collection. See [support and limitations](implementation-status.md) for what has actually been executed versus only configured in CI.
