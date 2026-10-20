PREFIX ?= /usr/local
DESTDIR ?=
RIVU_GIT_VERSION ?= $(shell git describe --long --tags --always --dirty 2>/dev/null || printf unknown)
CARGO_TARGET_DIR ?= .cache/target

export CARGO_TARGET_DIR

.PHONY: build lint clean install

build:
	RIVU_GIT_VERSION="$(RIVU_GIT_VERSION)" cargo build --workspace --release

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --all-features -- -D warnings

clean:
	cargo clean

install: build
	install -Dm755 "$(CARGO_TARGET_DIR)/release/rivu" "$(DESTDIR)$(PREFIX)/bin/rivu"
	install -Dm755 "$(CARGO_TARGET_DIR)/release/rivu-migrate" "$(DESTDIR)$(PREFIX)/bin/rivu-migrate"
	install -Dm644 data/applications/rivu.desktop "$(DESTDIR)$(PREFIX)/share/applications/rivu.desktop"
	install -Dm644 data/icons/rivu.svg "$(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/rivu.svg"
	install -Dm644 data/icons/rivu.png "$(DESTDIR)$(PREFIX)/share/icons/hicolor/256x256/apps/rivu.png"
