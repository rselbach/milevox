.PHONY: check check-all check-guis check-gui-shell check-shell format install \
	install-omarchy-gui package uninstall uninstall-omarchy-gui \
	validate-omarchy-gui

check: check-shell
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets

check-all: check check-guis

check-guis: check-gui-shell validate-omarchy-gui

check-shell:
	bash -n install.sh uninstall.sh scripts/download-model.sh \
		scripts/package-release.sh
	@if command -v shellcheck >/dev/null; then \
		shellcheck install.sh uninstall.sh scripts/download-model.sh \
			scripts/package-release.sh; \
	else \
		echo "shellcheck not installed; skipped"; \
	fi

check-gui-shell:
	bash -n guis/omarchy/install.sh guis/omarchy/uninstall.sh
	@if command -v shellcheck >/dev/null; then \
		shellcheck guis/omarchy/install.sh guis/omarchy/uninstall.sh; \
	else \
		echo "shellcheck not installed; skipped"; \
	fi

format:
	cargo fmt --all

validate-omarchy-gui:
	omarchy plugin validate ./guis/omarchy

install:
	./install.sh

install-omarchy-gui:
	./guis/omarchy/install.sh

package:
	./scripts/package-release.sh

uninstall:
	./uninstall.sh

uninstall-omarchy-gui:
	./guis/omarchy/uninstall.sh
