PREFIX ?= /usr/local
DESTDIR ?=

.PHONY: all clean install

all:
	cargo build --release

clean:
	cargo clean

install: all
	install -Dm755 target/release/rivu "$(DESTDIR)$(PREFIX)/bin/rivu"
	install -Dm644 assets/rivu.desktop "$(DESTDIR)$(PREFIX)/share/applications/rivu.desktop"
	install -Dm644 assets/rivu.svg "$(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/rivu.svg"
	install -Dm644 assets/rivu.png "$(DESTDIR)$(PREFIX)/share/icons/hicolor/256x256/apps/rivu.png"
