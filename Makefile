.PHONY: build install run clean

build:
	cargo build --release

# Binaries, the dbus/ activation files and the portals/ declaration are all
# enumerated by ccebuild, so nothing is named here.
install: build
	@command -v ccebuild >/dev/null || { echo "ccebuild not installed — run: make -C ../cce-compositor install"; exit 1; }
	ccebuild install --no-build cce-desktop-portal

run:
	cargo run

clean:
	cargo clean
