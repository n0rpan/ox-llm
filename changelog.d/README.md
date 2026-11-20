# Changelog fragments

Add one Markdown file here per PR, with a unique, descriptive lowercase name
such as `local-model-status.md`. Use lowercase letters, digits and single hyphens.
Write one or more Markdown bullets describing what the PR changes. Use absolute
URLs for links so they work in GitHub Release notes. Do not edit another PR's
fragment or append to `oxa/CHANGELOG.md`.

The release workflow reads the fragments from the tagged source, orders them by
filename, and adds them to the existing release notes. The one-time
`legacy-unreleased.md` fragment carries the previous pending entries.

After a stable release publishes, remove only the fragment files included in
that tag whose contents on `main` still match the tag. Leave newer or edited
fragments for the next release. Do not clear pending fragments for a prerelease.
The published notes and the tag retain the released entries; keep this README.
