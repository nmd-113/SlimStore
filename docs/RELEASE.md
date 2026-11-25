# Publishing SlimStore on GitHub

SlimStore uses a three-part Cargo version and Git tag, such as `1.0.1` and `v1.0.1`. The Windows file and product versions include a fourth component, such as `1.0.1.0`.

## Build and review

From the source directory, run the build and packaging scripts.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\build.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\verify.ps1 -Live
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\package.ps1
```

The GitHub workflow also builds the same three assets on a version tag or a manual run. It uploads them as a workflow artifact for review.

| Asset | Use |
| --- | --- |
| `dist/SlimStore.exe` | Portable Windows x64 app |
| `release/SlimStore-1.0.1-source.zip` | Matching SlimStore source, assets, build instructions, and license notices |
| `release/SHA256SUMS.txt` | SHA-256 hashes for the EXE and source ZIP |

## Create the release

1. Set the new Cargo and Windows manifest versions and move the unreleased changes into a versioned [changelog entry](../CHANGELOG.md). For each release, build and package from the same source revision.
2. Commit the project files, create the matching version tag, then push the commit and tag. The generated build, release, and local data folders are ignored by Git.
3. Create a draft release for the tag, use the changelog entry, and attach all three assets. Verify their hashes. If shipping a signed EXE, generate checksums after signing.
4. Make the matching source and required dependency source and notices available alongside the binary. [Third-party notices](THIRD-PARTY.md)
5. Publish the reviewed draft.

See [GitHub’s release guide](https://docs.github.com/en/repositories/releasing-projects-on-github/managing-releases-in-a-repository) for the draft and upload steps.
