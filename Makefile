PREFIX ?= /usr/local
PLIST_DIR = ~/Library/LaunchAgents
PLIST = com.johng.nfscache-mount.plist
LOG_DIR = ~/Library/Logs/nfscache

# NAS backing mount (override on the command line, e.g. `make remount NAS_HOST=...`).
# soft,timeo,retrans make the kernel return EIO instead of hanging when the VPN
# drops — the second half of the outage-resilience fix (nfscache's own circuit
# breaker is the first half).
NAS_HOST  ?= 192.168.50.21
NAS_SHARE ?= /volume1/media
NAS_MOUNT ?= $(HOME)/nas_media
NFS_OPTS  ?= vers=3,nolock,resvport,soft,timeo=30,retrans=2

.PHONY: build install uninstall service-start service-stop service-restart upgrade redeploy remount force-remount clean test

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

# Mount the NAS with resilient options if it isn't already mounted.
remount:
	@if mount | grep -q " $(NAS_MOUNT) "; then \
		echo "NAS already mounted: $(NAS_MOUNT)"; \
	else \
		echo "Mounting NAS (soft): $(NAS_HOST):$(NAS_SHARE) -> $(NAS_MOUNT)"; \
		mkdir -p $(NAS_MOUNT); \
		sudo mount -t nfs -o $(NFS_OPTS) $(NAS_HOST):$(NAS_SHARE) $(NAS_MOUNT); \
	fi

# Force a clean remount — use this to recover a hung/stale NAS mount.
force-remount:
	-sudo umount -f $(NAS_MOUNT) 2>/dev/null
	mkdir -p $(NAS_MOUNT)
	sudo mount -t nfs -o $(NFS_OPTS) $(NAS_HOST):$(NAS_SHARE) $(NAS_MOUNT)

# One-shot redeploy: rebuild, install, ensure NAS mounted (soft), restart service.
# Stops the service before overwriting the binary (avoids "text file busy").
redeploy: build
	-launchctl unload $(PLIST_DIR)/$(PLIST) 2>/dev/null
	sudo cp target/release/nfscache $(PREFIX)/bin/
	$(MAKE) remount
	mkdir -p $(LOG_DIR)
	cp launchd/$(PLIST) $(PLIST_DIR)/
	launchctl load $(PLIST_DIR)/$(PLIST)
	@echo "Redeployed. Verify with: nfscache status"

clean:
	cargo clean

test:
	cargo test
