- main support targets: ghostty, kitty, iterm2 (newer versions)
- you may add patches to the vendored opentui-native as needed.
- upon request to create a git commit, you should follow it with an update to CHANGELOG.md with a _brief_ message describing the commit along with its hash under the header at the top denoted `## Unreleased`. If said header does not exist, you should add it such that the CHANGELOG always looks like this:

```
# Changlog

## Unreleased

- Add xyz (`02f4f84`)

## vX.Y.Z — YYYY-MM-DD

...
```

Please read the changelog before making changes in it in order to understand the existing format and prose style.