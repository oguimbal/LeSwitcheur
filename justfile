default: check

check:
    cargo check --workspace --all-targets

fmt:
    cargo fmt --all

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

run:
    cargo run -p switcheur

# Watch-rebuild-restart loop for dev. Requires cargo-watch:
#   cargo install cargo-watch   (or brew install cargo-watch)
# `--open` reopens the switcher on every restart, no need to press the hotkey.
# If you kill the process by hand, cargo-watch will NOT restart it until you touch a file.
dev:
    cargo watch -c -w crates -x 'run -p switcheur -- --open'

build-release:
    cargo build --release -p switcheur

bundle: build-release
    ./bundle/bundle.sh

dmg: bundle
    ./bundle/dmg.sh

# Wipe saved settings + stale bundle, rebuild signed with the local self-signed
# identity, verify signature, launch. Handy to test a production build as if
# it were a fresh install.
test-bundle:
    ./scripts/test-bundle.sh

clean:
    cargo clean
    rm -rf dist
