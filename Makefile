PREFIX ?= /usr/local
PLIST_DIR = ~/Library/LaunchAgents
PLIST = com.johng.nfscache-mount.plist
LOG_DIR = ~/Library/Logs/nfscache

.PHONY: build install uninstall service-start service-stop service-restart upgrade clean test

build:
	cargo build --release

install: build
	sudo cp target/release/nfscache $(PREFIX)/bin/
	mkdir -p $(LOG_DIR)
	nfscache init

uninstall:
	-launchctl unload $(PLIST_DIR)/$(PLIST) 2>/dev/null
	rm -f $(PLIST_DIR)/$(PLIST)
	sudo rm -f $(PREFIX)/bin/nfscache

upgrade: build
	-launchctl unload $(PLIST_DIR)/$(PLIST) 2>/dev/null
	sudo cp target/release/nfscache $(PREFIX)/bin/
	-launchctl load $(PLIST_DIR)/$(PLIST) 2>/dev/null

service-start:
	mkdir -p $(LOG_DIR)
	cp launchd/$(PLIST) $(PLIST_DIR)/
	launchctl load $(PLIST_DIR)/$(PLIST)

service-stop:
	-launchctl unload $(PLIST_DIR)/$(PLIST) 2>/dev/null

service-restart: service-stop service-start

clean:
	cargo clean

test:
	cargo test
