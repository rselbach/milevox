.PHONY: check check-all check-guis check-gui-shell check-shell format install \
	install-omarchy-gui package package-arch package-core package-omarchy uninstall \
	uninstall-omarchy-gui \
	validate-omarchy-gui

check: check-shell
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets

check-all: check check-guis

check-guis: check-gui-shell validate-omarchy-gui

check-shell:
	bash -n install.sh uninstall.sh scripts/download-model.sh \
		scripts/setup-user.sh \
		scripts/package-arch-release.sh scripts/package-omarchy-release.sh \
		scripts/package-release.sh \
		guis/omarchy/milevox-omarchy
	@if command -v shellcheck >/dev/null; then \
		shellcheck install.sh uninstall.sh scripts/download-model.sh \
			scripts/setup-user.sh \
			scripts/package-arch-release.sh \
			scripts/package-omarchy-release.sh scripts/package-release.sh \
			guis/omarchy/milevox-omarchy; \
	else \
		echo "shellcheck not installed; skipped"; \
	fi

check-gui-shell:
	bash -n guis/omarchy/install.sh guis/omarchy/uninstall.sh \
		tests/omarchy-install.sh
	@if command -v shellcheck >/dev/null; then \
		shellcheck guis/omarchy/install.sh guis/omarchy/uninstall.sh \
			tests/omarchy-install.sh; \
	else \
		echo "shellcheck not installed; skipped"; \
	fi
	./tests/omarchy-install.sh

format:
	cargo fmt --all

validate-omarchy-gui:
	omarchy plugin validate ./guis/omarchy

install:
	./install.sh

install-omarchy-gui:
	./guis/omarchy/install.sh

package: package-arch

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

uninstall:
	./uninstall.sh

uninstall-omarchy-gui:
	./guis/omarchy/uninstall.sh
