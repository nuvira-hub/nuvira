# Contributing to NuviraHub

Thank you for wanting to help. Bug reports, ideas and translations are welcome in the [issues](https://github.com/nuvira-hub/nuvira/issues). For anything bigger than a small fix, please open an issue first, so we can agree on the approach before you spend time on it.

## Pull requests

- Code, comments, commit messages and documentation in English; interface texts in all five languages (`client/i18n/locales`), or in English and we will translate the others.
- Server: `go vet ./...` and `go test ./...` in `server/` must pass, code formatted with `gofmt`.
- App: `cargo fmt`, `cargo clippy -- -D warnings` and `cargo test` in `client/` must pass.
- One topic per pull request, with a description of what changes and how you tested it.

## Contributor licence agreement

NuviraHub is open source (AGPL-3.0-or-later) and is also the base of NuviraHub Pro, sold under a commercial licence. To be able to include contributions in both, every contributor agrees to the following when submitting a contribution (a pull request, a patch or a translation):

1. You keep the copyright of your contribution.
2. You grant Alessandro Blason, the copyright holder of NuviraHub, a perpetual, worldwide, non-exclusive, royalty-free and irrevocable licence to use, copy, modify, distribute and sublicense your contribution, also under licences other than the AGPL, including proprietary ones.
3. You confirm that the contribution is your own work, or that you have the right to submit it under these terms, and that it does not knowingly infringe anyone's rights.
4. Your contribution is in any case also available to everyone under the AGPL-3.0-or-later, like the rest of the project.

The first time you open a pull request you will be asked to confirm this agreement with a comment.

## Security problems

Do not report them in public issues: see [SECURITY.md](SECURITY.md).
