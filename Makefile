CARGO  ?= $(HOME)/.cargo/bin/cargo
PREFIX ?= /usr/local
BINDIR  = $(PREFIX)/bin

.PHONY: build install uninstall clean

build:
	$(CARGO) build --release

install:
	install -Dm755 target/release/white-noise-machine $(DESTDIR)$(BINDIR)/white-noise-machine

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/white-noise-machine

clean:
	$(CARGO) clean
