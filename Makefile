.DEFAULT_GOAL := help

CARGO ?= cargo
MISE ?= mise
PNPM ?= pnpm
RUSTUP ?= rustup
WINDOWS_TARGET ?= x86_64-pc-windows-msvc

.PHONY: help clean toolchain-install toolchain-check deps-install \
	format format-check rust-check clippy windows-clippy rust-test rust-build \
	ui-lint ui-typecheck ui-test ui-build ui-audit ui-outdated \
	rust-audit deny-check rust-outdated audit outdated build test \
	release-test release-version media-gate quality-check check

##@ Getting started
help: ## Show the documented Make targets
	@awk 'BEGIN { FS = ":.*##"; printf "Usage: make <target> [VAR=value...]\n" } /^##@/ { if (shown++) printf "\n"; printf "%s\n", substr($$0, 5); next } /^[a-zA-Z0-9_.-]+:.*##/ { printf "  \033[36m%-32s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

##@ Toolchain
toolchain-install: ## Install the repository-pinned Rust, Node, pnpm, and Cargo tools
	$(MISE) install --locked rust node pnpm cargo:cargo-audit cargo:cargo-outdated cargo:cargo-deny

toolchain-check: ## Verify active Rust, Node, and pnpm versions against repository pins
	@set -eu; \
	rust_version="$$($(MISE) exec -- rustc --version)"; \
	node_version="$$($(MISE) exec -- node --version)"; \
	pnpm_version="$$($(MISE) exec -- pnpm --version)"; \
	case "$$rust_version" in \
		"rustc 1.99.0 "*) ;; \
		*) echo "Expected Rust 1.99.0, got: $$rust_version" >&2; exit 1 ;; \
	esac; \
	test "$$node_version" = "v26.9.0" || { echo "Expected Node v26.9.0, got: $$node_version" >&2; exit 1; }; \
	test "$$pnpm_version" = "12.9.1" || { echo "Expected pnpm 12.9.1, got: $$pnpm_version" >&2; exit 1; }; \
	printf '%s; Node %s; pnpm %s\n' "$$rust_version" "$$node_version" "$$pnpm_version"

deps-install: ## Install frontend dependencies from the committed lockfile
	$(MISE) exec -- $(PNPM) install --frozen-lockfile

##@ Rust workspace
format: ## Format Rust workspace sources
	$(CARGO) fmt --all

format-check: ## Check Rust formatting without changing files
	$(CARGO) fmt --all --check

rust-check: ## Check every Rust workspace member
	$(CARGO) check --workspace

clippy: ## Run the CI Clippy gate with warnings denied
	$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings

windows-clippy: ## Cross-check the Windows-only crate used by CI
	$(RUSTUP) target add $(WINDOWS_TARGET)
	$(CARGO) clippy -p nian-platform-windows --target $(WINDOWS_TARGET) --all-targets -- -D warnings

rust-test: ## Run the Rust workspace tests
	$(CARGO) test --workspace

rust-build: ## Build the default Rust workspace members
	$(CARGO) build

##@ React UI
ui-lint: ## Lint the React UI
	$(MISE) exec -- $(PNPM) lint

ui-typecheck: ## Typecheck the React UI
	$(MISE) exec -- $(PNPM) typecheck

ui-test: ## Run the React UI tests
	$(MISE) exec -- $(PNPM) test

ui-build: ## Build the React UI
	$(MISE) exec -- $(PNPM) build

##@ Dependency maintenance
rust-audit: ## Check Cargo.lock against the RustSec advisory database
	$(MISE) exec -- cargo audit

deny-check: ## Check dependency licenses, advisories, sources, and bans
	$(MISE) exec -- cargo deny check --hide-inclusion-graph

ui-audit: ## Audit production pnpm dependencies
	$(MISE) exec -- $(PNPM) audit --prod

audit: ## Audit RustSec advisories and production UI dependencies
	$(MAKE) rust-audit
	$(MAKE) ui-audit

rust-outdated: ## List newer direct dependencies across Rust workspace members
	$(MISE) exec -- cargo outdated --workspace --root-deps-only

ui-outdated: ## List newer pnpm workspace dependencies
	PNPM="$(PNPM)" $(MISE) exec -- node scripts/run_pnpm_outdated.mjs

outdated: ## List newer direct dependencies in Rust and pnpm workspaces
	$(MAKE) rust-outdated
	$(MAKE) ui-outdated

##@ Build and release
build: ## Build the default Rust workspace members and React UI
	$(MAKE) rust-build
	$(MAKE) ui-build

test: ## Run Rust and UI test suites
	$(MAKE) rust-test
	$(MAKE) ui-test

release-test: ## Run release-script contract tests
	$(PNPM) release:test

release-version: ## Check synchronized application and release versions
	$(PNPM) release:version

media-gate: ## Validate the packaged media runtime contract
	$(PNPM) media:gate

##@ Quality
quality-check: ## Run the Rust and UI quality gates used by Forgejo CI
	$(MAKE) toolchain-check
	$(MAKE) format-check
	$(MAKE) rust-check
	$(MAKE) windows-clippy
	$(MAKE) clippy
	$(MAKE) rust-test
	$(MAKE) deny-check
	$(MAKE) ui-lint
	$(MAKE) ui-typecheck
	$(MAKE) ui-test
	$(MAKE) ui-build

check: quality-check ## Alias for quality-check

##@ Cleanup
clean: ## Remove Cargo and React UI build outputs
	$(CARGO) clean
	rm -rf ui/dist
