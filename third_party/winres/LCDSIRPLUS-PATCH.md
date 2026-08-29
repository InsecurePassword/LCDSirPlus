# LCDSirPlus winres patch

This directory vendors the build-required files from winres 0.1.12 under its
MIT license.

- Published crate SHA-256: b68db261ef59e9e52806f688020631e987592bd83619edccda9c47d42cde4f6c
- Published VCS commit: 2819c50fc9a035ea99679a2b3eef4e53fb4f3269
- Upstream lib.rs SHA-256: b5e5ddec2ff9a28d83b1f92bf7e752176bb39d27abefa60efc0db96acdbf929e
- Upstream LICENSE SHA-256: 3564da09711c475669c15346bb25a1530a7de683c13f4a51489c15a6d74438fa

Local changes replace the two VERSIONINFO HashMap values with BTreeMap,
retain VersionInfo's Hash implementation while adding ordering traits, expose
serialization to an in-memory writer internally, and test deterministic output
across 64 fixed insertion permutations. The public API and version remain
winres 0.1.12. Upstream pull request 50 was used as design inspiration only;
this source was not copied from its unmerged branch.
