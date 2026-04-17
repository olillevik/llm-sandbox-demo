# llm-box agent guidance

- product shape: `./llm-box copilot ...`
- current provider preset: `copilot`
- session is the primary unit of approval state
- interactive UX: terminal for provider, browser companion for approvals
- keep approvals file-based unless there is a clear reason to add a service layer
- avoid premature provider/plugin abstractions
- validate with `cargo test` and `bash ./tests/test_box.sh`
- dependency policy: `docs/adr/0001-dependency-policy.md`
