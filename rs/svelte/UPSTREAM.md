# Upstream snapshot

The Rust port is pinned to a specific upstream Svelte revision. When the user
says "update to latest upstream", the diff between this file and the new state
of `packages/svelte/` is the work that needs to be applied.

## Pinned revision

| Field | Value |
|---|---|
| Svelte package version | **5.55.7** |
| svelte-rs repo commit | `4d8f99a2709e3c02e48d8bc6c77458f4ba49d0e3` |
| Last upstream commit | `4d8f99a` — *Version Packages (#18220)* |
| Snapshot captured | 2026-05-15 |

## Pinned dependencies

The versions below are what `packages/svelte/package.json` declared at the
pinned commit. The Rust ports under `crates/` mirror **these** versions.

| npm package | Version | Rust crate | Status |
|---|---|---|---|
| `@jridgewell/remapping` | `^2.3.4` | (subsumed by `sourcemap` crate) | n/a |
| `@jridgewell/sourcemap-codec` | `^1.5.0` | (subsumed by `sourcemap` crate) | n/a |
| `@sveltejs/acorn-typescript` | `^1.0.5` | (subsumed by `oxc_parser`) | n/a |
| `@types/estree` | `^1.0.5` | (subsumed by `oxc_ast`) | n/a |
| `@types/trusted-types` | `^2.0.7` | (subsumed by `oxc_ast`) | n/a |
| `acorn` | `^8.12.1` | (replaced by `oxc_parser`) | n/a |
| `aria-query` | `5.3.1` (pinned) | `svelte_aria_data` | pending |
| `axobject-query` | `^4.1.0` | `svelte_aria_data` | pending |
| `clsx` | `^2.1.1` | (inline helper in `svelte_transform_shared`) | pending |
| `devalue` | `^5.8.1` | `svelte_devalue` | pending |
| `esm-env` | `^1.2.1` | (build-time constants) | n/a |
| `esrap` | `^2.2.4` | `svelte_codegen_js` | pending |
| `is-reference` | `^3.0.3` | `svelte_is_reference` | pending |
| `locate-character` | `^3.0.0` | (inline helper in `svelte_ast`) | pending |
| `magic-string` | `^0.30.11` | `svelte_magic_string` | pending |
| `zimmerframe` | `^1.1.2` | (per-crate visitor traits — no shared crate) | n/a |

### Open question — CSS parser choice

Plan R5 said "hand-port `read/style.js` to `svelte_css_parser`; don't use
lightningcss". That decision was made without measurement. **Revisit at Phase 2g.**
The concern was that lightningcss's selector AST might be too lossy for
Svelte's per-`RelativeSelector` / per-`SimpleSelector` scope-pruning needs.
But `parcel_selectors` may be composable enough to drive selector parsing
ourselves and only use lightningcss for `@rule`/declaration bodies — which
would save several hundred LOC and inherit a battle-tested CSS3+ parser.
Spike before committing.

## Codebase size snapshot (LOC)

These are taken from the pinned revision so we can detect drift after updates.

| Area | Files | LOC |
|---|---|---|
| `packages/svelte/src/compiler/**` | 243 | 40,773 |
| `packages/svelte/src/internal/**` (out of scope — runtime) | 117 | 20,048 |

## Test suite size snapshot

Fixture directories under `packages/svelte/tests/`, counted at the pinned commit.
Useful for spotting when upstream adds new fixtures we have not run yet.

| Suite | Fixtures |
|---|---|
| `parser-modern` | 24 |
| `parser-legacy` | 83 |
| `validator` | 325 |
| `compiler-errors` | 144 |
| `css` | 181 |
| `snapshot` | 32 |
| `sourcemaps` | 28 |
| `server-side-rendering` | 124 |
| `hydration` | 80 |
| `preprocess` | 19 |
| `print` | 40 |
| `migrate` | 76 |
| `runtime-runes` (smoke-only) | 975 |
| `runtime-legacy` (smoke-only) | 1,207 |

## How to refresh this snapshot

After pulling upstream changes and re-pointing the port:

```sh
# from repo root
git rev-parse HEAD
node -e "console.log(require('./packages/svelte/package.json').version)"
node -e "console.log(JSON.stringify(require('./packages/svelte/package.json').dependencies, null, 2))"
find packages/svelte/src/compiler -type f \( -name '*.js' -o -name '*.ts' \) | xargs wc -l | tail -1
for d in tests/parser-modern tests/parser-legacy tests/validator tests/compiler-errors \
         tests/css tests/snapshot tests/sourcemaps tests/server-side-rendering \
         tests/hydration tests/preprocess tests/print tests/migrate \
         tests/runtime-runes tests/runtime-legacy; do
  echo -n "$d: "; ls packages/svelte/$d/samples 2>/dev/null | wc -l | tr -d ' '
done
```
