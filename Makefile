.DEFAULT_GOAL := help

.PHONY: help
help: ## Show description of all commands
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

# --- Variables -----------------------------------------------------------------------------------

# The build target can be set via BUILD_TARGET to cross-compile.
# Used when targeting a specific environment (used to produce artifact binaries)
# in the CI.
ifneq ($(BUILD_TARGET),)
TARGET_FLAG = --target $(BUILD_TARGET)
endif

FEATURES_CLIENT=--features "std dap"
WARNINGS=RUSTDOCFLAGS="-D warnings"

TEST_MIDEN_NOTE_TRANSPORT_URL?=http://127.0.0.1:57292

# --- Linting -------------------------------------------------------------------------------------

.PHONY: clippy
clippy: ## Run Clippy with configs
	cargo clippy --workspace --features "testing std" --all-targets -- -D warnings

.PHONY: fix
fix: ## Run Fix with configs
	cargo fix --workspace --features "testing std" --all-targets --allow-staged --allow-dirty

.PHONY: format
format: ## Run format using nightly toolchain
	cargo +nightly fmt --all

.PHONY: format-check
format-check: ## Run format using nightly toolchain but only in check mode
	cargo +nightly fmt --all --check

.PHONY: shear
shear: ## Run cargo-shear to find unused or misplaced dependencies
	cargo shear

.PHONY: lint
lint: fix format toml clippy typos-check shear ## Run all linting tasks at once (clippy, fixing, formatting, typos)

.PHONY: toml
toml: ## Runs Format for all TOML files
	taplo fmt

.PHONY: toml-check
toml-check: ## Runs Format for all TOML files but only in check mode
	taplo fmt --check --verbose

.PHONY: typos-check
typos-check: ## Run typos to check for spelling mistakes
	@typos --config ./.typos.toml

# --- Documentation -------------------------------------------------------------------------------

.PHONY: doc
doc: ## Generate & check rust documentation. Ensure you have the nightly toolchain installed.
	@cd crates/rust-client && \
	RUSTDOCFLAGS="-D warnings --cfg docsrs" cargo +nightly doc --lib --no-deps --all-features --keep-going --release

doc-open: ## Generate & open rust documentation in browser. Ensure you have the nightly toolchain installed.
	@cd crates/rust-client && \
	RUSTDOCFLAGS="-D warnings --cfg docsrs" cargo +nightly doc --lib --no-deps --all-features --keep-going --release --open

.PHONY: serve-docs
serve-docs: ## Serves the docs
	cd docs/external && npm run start:dev

# --- Testing -------------------------------------------------------------------------------------

.PHONY: test
test: ## Run tests
	cargo nextest run --workspace --release --lib $(FEATURES_CLIENT)

.PHONY: test-miden-bench
test-miden-bench: ## Run miden-bench CLI tests
	cargo nextest run --package miden-client-bench --release --test=integration

.PHONY: test-docs
test-docs: ## Run documentation tests
	cargo test --doc $(FEATURES_CLIENT)

# --- Integration testing -------------------------------------------------------------------------

.PHONY: start-node
start-node: ## Start the testing node in the foreground, streaming logs (Ctrl+C stops it)
	./scripts/start-test-node.sh

.PHONY: start-node-agglayer
start-node-agglayer: ## Start the testing node with agglayer genesis accounts
	AGGLAYER_GENESIS=1 ./scripts/start-test-node.sh

.PHONY: start-node-background
start-node-background: ## Start the testing node in the background
	./scripts/start-test-node.sh --background

.PHONY: stop-node
stop-node: ## Stop the testing node
	./scripts/stop-test-node.sh

.PHONY: start-note-transport-background
start-note-transport-background: ## Start the note transport service in background
	./scripts/start-note-transport-bg.sh

.PHONY: stop-note-transport
stop-note-transport: ## Stop the note transport service
	./scripts/stop-note-transport.sh

.PHONY: start-note-transport
start-note-transport:
	./scripts/start-note-transport.sh

.PHONY: integration-test
integration-test: ## Run integration tests
	cargo nextest run --workspace --release --test=integration

.PHONY: integration-test-full
integration-test-full: ## Run the integration test binary with ignored tests included (requires note transport service)
	TEST_MIDEN_NOTE_TRANSPORT_URL=$(TEST_MIDEN_NOTE_TRANSPORT_URL) cargo nextest run --workspace --release --test=integration
	cargo nextest run --workspace --release --test=integration --run-ignored ignored-only -- import_genesis_accounts_can_be_used_for_transactions

.PHONY: integration-test-miden-bench
integration-test-miden-bench: install-bench ## Run miden-bench smoke tests
	./scripts/test-miden-bench-smoke.sh

.PHONY: test-dev
test-dev: ## Run tests with debug assertions enabled via test-dev profile
	cargo nextest run --workspace --cargo-profile test-dev --lib $(FEATURES_CLIENT)

.PHONY: integration-test-dev
integration-test-dev: ## Run integration tests with debug assertions enabled via test-dev profile
	cargo nextest run --workspace --cargo-profile test-dev --test=integration

.PHONY: integration-test-binary
integration-test-binary: ## Run the integration tests using the standalone binary (requires note transport service)
	TEST_MIDEN_NOTE_TRANSPORT_URL=$(TEST_MIDEN_NOTE_TRANSPORT_URL) cargo run --package miden-client-integration-tests --release --locked

# --- Installing ----------------------------------------------------------------------------------

install: ## Install the CLI binary
	cargo install --path bin/miden-cli --locked

install-bench: ## Install the benchmark binary
	cargo install --path bin/miden-bench --locked

install-tests: ## Install the tests binary
	cargo install --path bin/integration-tests --locked

# --- Building ------------------------------------------------------------------------------------

build: ## Build the CLI binary, client library and tests binary in release mode
	cargo build --workspace $(TARGET_FLAG) --release --locked

.PHONY: build-wasm
build-wasm: ## Build the client library for wasm32 with no_std (no default features)
	cargo build --package miden-client --target wasm32-unknown-unknown --no-default-features --locked

# --- Check ---------------------------------------------------------------------------------------

.PHONY: check
check: ## Build the CLI binary and client library in release mode
	cargo check --workspace --release

## --- Setup --------------------------------------------------------------------------------------

.PHONY: check-tools
check-tools: ## Checks if development tools are installed
	@echo "Checking development tools..."
	@command -v mdbook        >/dev/null 2>&1 && echo "[OK] mdbook is installed"        || echo "[MISSING] mdbook       (make install-tools)"
	@command -v typos         >/dev/null 2>&1 && echo "[OK] typos is installed"         || echo "[MISSING] typos        (make install-tools)"
	@command -v cargo nextest >/dev/null 2>&1 && echo "[OK] cargo-nextest is installed" || echo "[MISSING] cargo-nextest(make install-tools)"
	@command -v cargo-shear   >/dev/null 2>&1 && echo "[OK] cargo-shear is installed"   || echo "[MISSING] cargo-shear  (make install-tools)"
	@command -v taplo         >/dev/null 2>&1 && echo "[OK] taplo is installed"         || echo "[MISSING] taplo        (make install-tools)"

.PHONY: install-tools
install-tools: ## Installs Rust tools required by the Makefile
	@echo "Installing development tools..."
	@rustup show active-toolchain >/dev/null 2>&1 || (echo "Rust toolchain not detected. Install rustup + toolchain first." && exit 1)
	@RUST_TC=$$(rustup show active-toolchain | awk '{print $$1}'); \
		echo "Ensuring required Rust components are installed for $$RUST_TC..."; \
		rustup component add --toolchain $$RUST_TC clippy rust-src rustfmt >/dev/null
	# Rust-related
	cargo install mdbook --locked
	cargo install typos-cli@1.42.3 --locked
	cargo install cargo-nextest@0.9.128 --locked
	cargo install cargo-shear@1.11.2 --locked
	cargo install taplo-cli --locked
	@echo "Development tools installation complete!"
