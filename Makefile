.PHONY: audit-rust check check-all check-ci check-core-ci check-gui-behavior \
	check-gui-ci check-guis check-gui-shell check-metadata check-qml \
	check-release-metadata check-shell check-systemd format install \
	install-omarchy-gui package package-arch package-core package-omarchy \
	package-smoke prepare-release uninstall uninstall-omarchy-gui \
	validate-omarchy-gui

CORE_TESTS = tests/audit-config.sh tests/check-abi.sh \
	tests/package-content.sh tests/package-lifecycle.sh \
	tests/prepare-release.sh tests/release-helpers.sh

check: check-shell
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets

check-all: check check-guis

check-ci: check-core-ci check-gui-ci

check-core-ci: check-shell check-metadata check-systemd
	cargo fmt --all -- --check
	cargo clippy --locked --all-targets -- -D warnings
	cargo test --locked --all-targets
	@for test in $(CORE_TESTS); do "$$test"; done

check-gui-ci: check-gui-behavior

check-gui-behavior: check-gui-shell check-qml check-release-metadata

check-guis: check-gui-behavior validate-omarchy-gui

check-metadata check-release-metadata:
	@package_version="$$(awk -F '"' \
		'/^version = / { print $$2; exit }' Cargo.toml)"; \
	bash -c 'source scripts/lib-release.sh; validate_version "$$1"' _ \
		"$${package_version}" || { echo "invalid package version: $${package_version}" >&2; exit 1; }; \
	manifest_version="$$(jq -r '.version // empty' \
		guis/omarchy/manifest.json)"; \
	if [ "$${manifest_version}" != "$${package_version}" ]; then \
		echo "Omarchy manifest version $${manifest_version}" \
			"does not match $${package_version}" >&2; \
		exit 1; \
	fi

check-shell:
	@files="$$(find . -type f \( -name '*.sh' -o -path './guis/omarchy/milevox-omarchy' \) -print)"; \
		fragments="$$(find packaging -type f -name '*.install' -print)"; \
		bash -n $$files $$fragments; \
		command -v shellcheck >/dev/null || { echo 'shellcheck is required' >&2; exit 1; }; \
		shellcheck -x $$files; shellcheck -x --shell=bash $$fragments

check-gui-shell:
	bash -n guis/omarchy/install.sh guis/omarchy/uninstall.sh \
		tests/omarchy-install.sh
	./tests/omarchy-install.sh

check-qml:
	bash tests/qml-plain-text.sh

check-systemd:
	MILEVOX_VERIFY_SYSTEMD=1 ./tests/systemd-service.sh

audit-rust:
	./scripts/audit-rust.sh

format:
	cargo fmt --all

validate-omarchy-gui:
	omarchy plugin validate ./guis/omarchy

install:
	./install.sh

install-omarchy-gui:
	./guis/omarchy/install.sh

package: package-core package-omarchy

package-arch: package-core package-omarchy
	./scripts/package-arch-release.sh milevox \
		dist/milevox-linux-$$(uname -m).tar.gz
	./scripts/package-arch-release.sh milevox-omarchy \
		dist/milevox-omarchy-$$(awk -F '"' \
		'/^version = / { print $$2; exit }' Cargo.toml).tar.gz

package-core:
	./scripts/package-release.sh

package-omarchy:
	./scripts/package-omarchy-release.sh

package-smoke: package
	./scripts/check-release-archives.sh \
		dist/milevox-linux-$$(uname -m).tar.gz \
		dist/milevox-omarchy-$$(awk -F '"' \
		'/^version = / { print $$2; exit }' Cargo.toml).tar.gz

prepare-release:
	./scripts/prepare-release.sh "$(VERSION)"

uninstall:
	./uninstall.sh

uninstall-omarchy-gui:
	./guis/omarchy/uninstall.sh
