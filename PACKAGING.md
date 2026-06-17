Packaging
=========

This file contains quick reminders and notes on how to package Flavio.

We consider here the packaging flow of Flavio version `1.0.0` for Linux.

1. **How to bump Flavio version before a release:**
    1. Bump version in `Cargo.toml` to `1.0.0`
    2. Execute `cargo update` to bump `Cargo.lock`
    3. Bump Debian package version in `debian/rules` to `1.0.0`

2. **How to build Flavio, package it and release it on Crates, GitHub and Docker Hub (multiple architectures):**
    1. Tag the latest Git commit corresponding to the release with tag `v1.0.0`, and push the tag
    2. Wait for all release jobs to complete on the [actions](https://github.com/crisp-oss/flavio/actions) page on GitHub
    3. Publish a changelog and upload all the built archives on the [releases](https://github.com/crisp-oss/flavio/releases) page on GitHub
