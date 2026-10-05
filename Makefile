PREFIX ?= /usr/local
DESTDIR ?=
RIVU_GIT_VERSION ?= $(shell git describe --long --tags --always --dirty 2>/dev/null || printf unknown)

.PHONY: build clean install

build:
	RIVU_GIT_VERSION="$(RIVU_GIT_VERSION)" cargo build --release --features ffmpeg

clean:
	cargo clean

install: build
	install -Dm755 target/release/rivu "$(DESTDIR)$(PREFIX)/bin/rivu"
	install -Dm644 assets/rivu.desktop "$(DESTDIR)$(PREFIX)/share/applications/rivu.desktop"
	install -Dm644 assets/rivu.svg "$(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/rivu.svg"
	install -Dm644 assets/rivu.png "$(DESTDIR)$(PREFIX)/share/icons/hicolor/256x256/apps/rivu.png"
