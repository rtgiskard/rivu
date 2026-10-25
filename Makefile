PREFIX ?= /usr/local
DESTDIR ?=
RIVU_GIT_VERSION ?= $(shell git describe --long --tags --always --dirty 2>/dev/null || printf unknown)
CARGO_TARGET_DIR ?= .cache/target
PACKAGE_NAME := rivu-linux-$(shell uname -m)
DIST_DIR ?= .cache/dist

export CARGO_TARGET_DIR
# Keep DESTDIR for application installation, not Cargo/CMake dependency builds.
unexport DESTDIR

.PHONY: build lint ui-demo clean install package

build:
	RIVU_GIT_VERSION="$(RIVU_GIT_VERSION)" cargo build --workspace --release

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --all-features -- -D warnings

ui-demo:
	cargo run -p rivu-ui --example showcase --features showcase

clean:
	cargo clean

install: build
	install -Dm755 "$(CARGO_TARGET_DIR)/release/rivu" "$(DESTDIR)$(PREFIX)/bin/rivu"
	install -Dm755 "$(CARGO_TARGET_DIR)/release/rivu-migrate" "$(DESTDIR)$(PREFIX)/bin/rivu-migrate"
	install -Dm644 data/icons/rivu.svg "$(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/rivu.svg"
	install -Dm644 data/icons/rivu.png "$(DESTDIR)$(PREFIX)/share/icons/hicolor/256x256/apps/rivu.png"
	install -Dm644 data/applications/rivu.desktop "$(DESTDIR)$(PREFIX)/share/applications/rivu.desktop"

package:
	mkdir -p .cache "$(DIST_DIR)"
	set -eu; \
	stage=$$(mktemp -d "$(abspath .cache)/package.XXXX"); \
	trap 'rm -rf "$$stage"' EXIT; \
	trap 'exit 1' HUP INT TERM; \
	$(MAKE) install PREFIX= DESTDIR="$$stage/$(PACKAGE_NAME)"; \
	{ \
		printf 'Build: %s\nFeatures: ffmpeg (default); no embedded font\n' "$(RIVU_GIT_VERSION)"; \
		printf 'RUSTFLAGS: %s\n' "$${RUSTFLAGS:-<unset>}"; \
		rustc --version; \
		getconf GNU_LIBC_VERSION; \
		for binary in rivu rivu-migrate; do \
			printf '\n%s shared libraries:\n' "$$binary"; \
			ldd "$$stage/$(PACKAGE_NAME)/bin/$$binary"; \
		done; \
	} > "$$stage/$(PACKAGE_NAME)/build-info.txt"; \
	tar -C "$$stage" --zstd -cf "$(DIST_DIR)/$(PACKAGE_NAME).tar.zst" "$(PACKAGE_NAME)"; \
	cd "$(DIST_DIR)" && sha256sum "$(PACKAGE_NAME).tar.zst" > "$(PACKAGE_NAME).tar.zst.sha256"
