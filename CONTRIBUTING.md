# Contributing to Arrowhead

Thank you for your interest in improving Arrowhead! We aim to keep the project approachable for external contributors while maintaining a reliable toolchain for Obsidian users.

## Getting Started

- Read the top-level [`README`](README.md) for architecture context.
- Review the [feature development guide](docs/feature_development_guide.md) and the agent playbook in [`AGENTS.md`](AGENTS.md) for coding conventions.
- Fork the repository and create a feature branch for your work.

## Development Workflow

1. Install Rust 1.86 (2024 edition). The pinned toolchain is defined in [`rust-toolchain.toml`](rust-toolchain.toml).
2. Run the standard checks before opening a pull request:

   ```bash
   cargo fmt
   cargo check --all-targets
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```

3. Add unit tests next to the code they cover. Integration tests will be added in a future milestone—design new APIs so they remain testable when that harness arrives.
4. Make sure new behaviour is documented. Update relevant sections in `README.md` or `docs/` when the user experience changes.

## Commit & PR Guidelines

- Use clear commit messages in the form `type: imperative summary` (e.g. `fix: guard daemon startup errors`).
- Keep pull requests focused. Separate unrelated changes into distinct PRs.
- Include a short summary of behaviour changes and tests executed in the PR description.

## Reporting Issues

If you encounter a bug or have a feature request, open an issue that includes:

- A clear title and description.
- Steps to reproduce the problem.
- Expected vs. actual behaviour.
- Relevant logs or stack traces (redact sensitive data).

Security-sensitive reports should follow the process in [`SECURITY.md`](SECURITY.md).

## License

By contributing to Arrowhead you agree that your contributions will be licensed under the MIT License. See [`LICENSE`](LICENSE) for the full text.
