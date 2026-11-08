PREFIX ?= /usr/local
DESTDIR ?=
PYTHON ?= python3
RIVU_GIT_VERSION ?= $(shell git describe --long --tags --always --dirty 2>/dev/null || printf unknown)
CARGO_TARGET_DIR ?= .cache/target
PACKAGE_NAME := rivu-linux-$(shell uname -m)
DIST_DIR ?= .cache/dist
PROCESSING_RESOURCE_DIR := $(DESTDIR)$(PREFIX)/share/rivu/audio_processing
PROCESSING_SOURCES := install.py prepare.py provision.py onnx_speech.py df3.py
PROCESSING_RUNTIME := $(wildcard tools/audio_processing/runtime/*.py tools/audio_processing/runtime/*.schema.json)

export CARGO_TARGET_DIR
# Keep DESTDIR for application installation, not Cargo/CMake dependency builds.
unexport DESTDIR

.PHONY: build lint test test-rust test-python ui-demo clean install install-only package

build:
	RIVU_GIT_VERSION="$(RIVU_GIT_VERSION)" cargo build --locked --workspace --release

lint:
	cargo fmt --all -- --check
	cargo clippy --locked --workspace --all-targets --all-features -- -D warnings

test: test-rust test-python

test-rust:
	cargo test --locked --workspace
	cargo test --locked --workspace --all-features
	cargo test --locked --workspace --no-default-features
	cargo test --locked -p rivu --no-default-features --features gui
	cargo test --locked -p rivu --no-default-features --features tui
	cargo test --locked -p rivu-dsp --no-default-features
	cargo test --locked -p rivu-format --no-default-features
	cargo test --locked -p rivu-utils --no-default-features
	cargo test --locked -p rivu-utils --no-default-features --features logging
	cargo test --locked -p rivu-utils --no-default-features --features terminal
	cargo test --locked -p rivu-utils --all-features

test-python:
	$(PYTHON) -c 'import numpy, scipy, soundfile, jsonschema'
	$(PYTHON) -m unittest discover -s tools/audio_processing -p 'test_*.py'

ui-demo:
	cargo run -p rivu-ui --example showcase --features showcase

clean:
	cargo clean

install: build
	$(MAKE) install-only

install-only:
	install -Dm755 "$(CARGO_TARGET_DIR)/release/rivu" "$(DESTDIR)$(PREFIX)/bin/rivu"
	install -Dm755 "$(CARGO_TARGET_DIR)/release/rivu-migrate" "$(DESTDIR)$(PREFIX)/bin/rivu-migrate"
	install -Dm644 data/icons/rivu.svg "$(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/rivu.svg"
	install -Dm644 data/icons/rivu.png "$(DESTDIR)$(PREFIX)/share/icons/hicolor/256x256/apps/rivu.png"
	install -Dm644 data/applications/rivu.desktop "$(DESTDIR)$(PREFIX)/share/applications/rivu.desktop"
	# Audio-processing preparation and runtime resources
	install -d "$(PROCESSING_RESOURCE_DIR)/runtime"
	install -m644 $(addprefix tools/audio_processing/,$(PROCESSING_SOURCES)) "$(PROCESSING_RESOURCE_DIR)/"
	install -m644 $(PROCESSING_RUNTIME) "$(PROCESSING_RESOURCE_DIR)/runtime/"

package:
	mkdir -p .cache "$(DIST_DIR)"
	set -eu; \
	stage=$$(mktemp -d "$(abspath .cache)/package.XXXX"); \
	trap 'rm -rf "$$stage"' EXIT; \
	trap 'exit 1' HUP INT TERM; \
	$(MAKE) install PREFIX= DESTDIR="$$stage/$(PACKAGE_NAME)"; \
	{ \
		printf 'Build: %s\nFeatures: gui, tui, ffmpeg, onnx (default); no embedded font\n' "$(RIVU_GIT_VERSION)"; \
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
