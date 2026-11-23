# Fuzzing

Ondura Server parses several formats straight from untrusted input — WBXML
(ActiveSync), SOAP/POX XML (EWS and Autodiscover), MIME (inbound SMTP) and
iCalendar (meeting invitations). These are the server's primary attack surface,
so they are exercised two ways.

## 1. Randomised smoke tests (stable, run in CI)

Each parser (and the collab OT operation path) has a randomised smoke test that
throws many random and mutated inputs at it and asserts it never panics,
overflows the stack, or hangs — it may only return a value or an error. These run
as ordinary tests on stable Rust, so they are part of `cargo test` and the CI
pipeline:

```sh
cargo test --test fuzz_smoke
```

They are a safety net, not a replacement for coverage-guided fuzzing: random
mutation reaches shallow states quickly but explores deep parser logic poorly.

## 2. Coverage-guided fuzzing (nightly, run on demand)

The `fuzz/` crate holds [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz)
targets driven by libFuzzer. libFuzzer needs a nightly toolchain, so the crate
is excluded from the workspace and built on its own.

Setup:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

Available targets:

| Target          | Parser under test                         |
| --------------- | ----------------------------------------- |
| `wbxml_decode`  | `ondura_wbxml::Element::decode`           |
| `ews_xml`       | `ondura_server::xml::Node::parse`         |
| `mime_meta`     | `ondura_server::mime::parse_meta`         |
| `ical_parse`    | `ondura_server::ical::parse`              |
| `collab_ot`     | `ondura_server::collab::ot` (operation parse, apply, transform) |

Run one (from the repository root):

```sh
cargo +nightly fuzz run wbxml_decode
```

List them with `cargo +nightly fuzz list`. A crash is written to
`fuzz/artifacts/<target>/` with the input that triggered it; reproduce it with:

```sh
cargo +nightly fuzz run wbxml_decode fuzz/artifacts/wbxml_decode/crash-<hash>
```

When a crash is found, add its reproducer to `crates/<crate>/tests/` as a
regression test and fix the parser. Keep an interesting-input corpus under
`fuzz/corpus/<target>/` so later runs start from known-deep states.

### Recommended periodic run

Coverage-guided fuzzing benefits from long runs. A practical cadence is a timed
run of each target on the build server, e.g. ten minutes each:

```sh
for t in $(cargo +nightly fuzz list); do
  cargo +nightly fuzz run "$t" -- -max_total_time=600
done
```
