# Contributing to NuviraHub

Thank you for wanting to help. Bug reports, ideas and translations are welcome in the [issues](https://github.com/nuvira-hub/nuvira/issues). For anything bigger than a small fix, please open an issue first, so we can agree on the approach before you spend time on it.

## Pull requests

- Code, comments, commit messages and documentation in English; interface texts in all five languages (`client/i18n/locales`), or in English and we will translate the others.
- Server: `go vet ./...` and `go test ./...` in `server/` must pass, code formatted with `gofmt`.
- App: `cargo fmt`, `cargo clippy -- -D warnings` and `cargo test` in `client/` must pass.
- One topic per pull request, with a description of what changes and how you tested it.

## Contributor licence agreement

NuviraHub is open source (AGPL-3.0-or-later) and is also the base of NuviraHub Pro, sold under a commercial licence. To be able to include contributions in both, every contributor accepts the **[Contributor Licence Agreement](CLA.md)** once. In short:

1. You keep the copyright of your contribution.
2. You grant Alessandro Blason, the copyright holder of NuviraHub, a perpetual, worldwide, non-exclusive, royalty-free and irrevocable licence to use, modify, distribute and sublicense it, also under licences other than the AGPL, and a licence to the patents your contribution needs.
3. You confirm that it is your own work, or that you have the right to submit it (also towards your employer).
4. Every contribution included in NuviraHub is available to everyone under the AGPL-3.0-or-later, like the rest of the project.

The full text in [CLA.md](CLA.md) is what counts. On your first pull request a bot asks you to accept it with a comment; it then applies to all your contributions.

## Security problems

Do not report them in public issues: see [SECURITY.md](SECURITY.md).

