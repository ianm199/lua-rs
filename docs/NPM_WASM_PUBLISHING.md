# Publishing `lua-rs-wasm` to npm

Status: the package is built, packed, tarball-installed, and smoke-tested by
`./harness/check_wasm_package.sh`. The manual `Publish lua-rs-wasm` GitHub
Actions workflow has also passed in dry-run mode on `main`.

The package has not been published until `npm view lua-rs-wasm version` returns
a version instead of 404.

## Ownership Decision

The first publish claims the `lua-rs-wasm` package name for the npm account or
organization whose token is used. Do not publish from a personal or company npm
account unless that is the intended owner.

The current local machine is logged into npm as `jonathan-prizeout`; direct local
publishing would claim the package under that account.

## Preferred Path: GitHub Actions

1. Create an npm token for the account or organization that should own
   `lua-rs-wasm`.
2. Add it to this repository as `NPM_TOKEN`:

   ```bash
   gh secret set NPM_TOKEN
   ```

3. Run the workflow dry-run from `main`:

   ```bash
   gh workflow run "Publish lua-rs-wasm" --ref main -f dry_run=true -f tag=latest
   gh run watch
   ```

4. Publish for real:

   ```bash
   gh workflow run "Publish lua-rs-wasm" --ref main -f dry_run=false -f tag=latest
   gh run watch
   ```

5. Verify the registry state:

   ```bash
   npm view lua-rs-wasm version
   npm view lua-rs-wasm dist.tarball
   npm run test:registry --prefix packages/lua-rs-wasm
   ```

The workflow runs `WASM_SKIP_BROWSER=1 ./harness/check_wasm_package.sh` before
publishing, then runs `npm publish --provenance --access public --tag <tag>` from
`packages/lua-rs-wasm`. On real publishes, it also runs the registry smoke after
publishing: a fresh temporary app installs `lua-rs-wasm@<package version>` from
npm, imports `lua-rs-wasm/node`, and executes Lua through the packaged `.wasm`.
That smoke retries installation for npm registry propagation.

## Alternative: Trusted Publishing

npm also supports trusted publishing from GitHub Actions, which avoids storing
an npm token and automatically creates provenance attestations. That requires
configuring the package's trusted publisher settings on npm before publishing.
Until that is configured, use the `NPM_TOKEN` workflow above.
