# Every target runs inside Docker. Do not run cargo on the host.
export UID := $(shell id -u)
export GID := $(shell id -g)
RUN = docker compose run --rm dev

# Product identity comes from brand.toml (single source of truth); see docs/TECHNICAL_DECISIONS.md.
brand_get = $(shell sed -n '/^\[$(1)\]/,/^\[/s/^$(2)[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' brand.toml)
BRAND_NAME = $(call brand_get,app,name)
BRAND_SLUG = $(call brand_get,app,slug)
BRAND_VERSION = $(call brand_get,app,version)
BIN_GUI = $(call brand_get,bins,gui)
BIN_TUI = $(call brand_get,bins,tui)
BIN_TRAY = $(call brand_get,bins,tray)
BIN_DAEMON = $(call brand_get,bins,daemon)
BIN_CLI = $(call brand_get,bins,cli)

.PHONY: image shell build test fmt fmt-check clippy release dist gui tui daemon check brand-test install-test
image:
	docker compose build dev
shell:
	$(RUN) bash
build:
	$(RUN) cargo build --workspace
test:
	$(RUN) cargo test --workspace
fmt:
	$(RUN) cargo fmt --all
fmt-check:
	$(RUN) cargo fmt --all -- --check
clippy:
	$(RUN) cargo clippy --workspace --all-targets -- -D warnings
release:
	$(RUN) cargo build --workspace --release
# Release binaries for running on the host (built in Docker, copied to ./dist under the names in brand.toml,
# plus systemd/.desktop files rendered from contrib/).
dist:
	$(RUN) cargo build --release -p plan-burner-app -p plan-burner-tui -p plan-burner-tray -p plan-burner-cli -p plan-burner-daemon
	mkdir -p dist
	$(RUN) sh -c 'cp /home/dev/target/release/gui "/work/dist/$(BIN_GUI)" && cp /home/dev/target/release/tui "/work/dist/$(BIN_TUI)" && cp /home/dev/target/release/tray "/work/dist/$(BIN_TRAY)" && cp /home/dev/target/release/cli "/work/dist/$(BIN_CLI)" && cp /home/dev/target/release/daemon "/work/dist/$(BIN_DAEMON)"'
	$(RUN) sh -c '/home/dev/target/release/tray --export-icon "/work/dist/$(BRAND_SLUG).png" --size 256'
	sed -e 's|@NAME@|$(BRAND_NAME)|g' -e 's|@SLUG@|$(BRAND_SLUG)|g' -e 's|@VERSION@|$(BRAND_VERSION)|g' \
	    -e 's|@BIN_GUI@|$(BIN_GUI)|g' -e 's|@BIN_TUI@|$(BIN_TUI)|g' -e 's|@BIN_TRAY@|$(BIN_TRAY)|g' -e 's|@BIN_DAEMON@|$(BIN_DAEMON)|g' \
	    contrib/systemd.service.in > "dist/$(BRAND_SLUG)-daemon.service"
	sed -e 's|@NAME@|$(BRAND_NAME)|g' -e 's|@SLUG@|$(BRAND_SLUG)|g' -e 's|@VERSION@|$(BRAND_VERSION)|g' \
	    -e 's|@BIN_GUI@|$(BIN_GUI)|g' -e 's|@BIN_TUI@|$(BIN_TUI)|g' -e 's|@BIN_TRAY@|$(BIN_TRAY)|g' -e 's|@BIN_DAEMON@|$(BIN_DAEMON)|g' \
	    contrib/desktop.in > "dist/$(BRAND_SLUG).desktop"
	sed -e 's|@NAME@|$(BRAND_NAME)|g' -e 's|@SLUG@|$(BRAND_SLUG)|g' -e 's|@VERSION@|$(BRAND_VERSION)|g' \
	    -e 's|@BIN_GUI@|$(BIN_GUI)|g' -e 's|@BIN_TUI@|$(BIN_TUI)|g' -e 's|@BIN_TRAY@|$(BIN_TRAY)|g' -e 's|@BIN_DAEMON@|$(BIN_DAEMON)|g' \
	    contrib/tray.service.in > "dist/$(BRAND_SLUG)-tray.service"
	printf 'name=%s\nslug=%s\nversion=%s\nbin_gui=%s\nbin_tui=%s\nbin_tray=%s\nbin_cli=%s\nbin_daemon=%s\n' \
	    '$(BRAND_NAME)' '$(BRAND_SLUG)' '$(BRAND_VERSION)' '$(BIN_GUI)' '$(BIN_TUI)' '$(BIN_TRAY)' '$(BIN_CLI)' '$(BIN_DAEMON)' > dist/MANIFEST

# Proves the identity is injected: build the TUI against a different brand file and check --version.
brand-test:
	$(RUN) sh -c 'sed -e "s/^name = .*/name = \"Test Brand\"/" -e "s/^slug = .*/slug = \"test-brand\"/" brand.toml > /tmp/brand-test.toml && BRAND_FILE=/tmp/brand-test.toml CARGO_TARGET_DIR=/tmp/brand-target cargo run -q -p plan-burner-tui -- --version | grep -qx "Test Brand $(BRAND_VERSION)" && echo brand-test: ok'

# Requires X access for the container; grant it yourself (e.g. `xhost +SI:localuser:$(whoami)`) if needed.
gui:
	docker compose --profile gui run --rm gui
# Terminal UI inside the container (needs a TTY; `docker compose run` allocates one).
daemon:
	$(RUN) cargo run -p plan-burner-daemon
tui:
	$(RUN) cargo run -p plan-burner-tui
# Runs install.sh against a tarball packed from `make dist` output (throwaway HOME, no network, no systemd).
install-test: dist
	$(RUN) sh -n install.sh
	$(RUN) sh tests/install_test.sh dist
check: fmt-check clippy test
